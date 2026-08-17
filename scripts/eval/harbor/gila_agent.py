"""Harbor installed-agent adapter for native ``gila solve`` runs."""

from __future__ import annotations

import hashlib
import json
import posixpath
import re
import shlex
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, override

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

_OUTCOMES = {
    "completed",
    "model_error",
    "transport_error",
    "timeout",
    "harness_error",
}
_SHA256 = re.compile(r"^(?:sha256:)?[0-9a-f]{64}$")
_VERSION = re.compile(r"^\d+\.\d+\.\d+(?:[-+][^ ]+)? \([0-9a-f]{12}\)$")
_AIRFRAME_REVISION = "8cde29f0b16cab9206fb76d94925b1ea49ee68bc"
_EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


@dataclass(frozen=True)
class BenchSettings:
    binary: Path
    profile: Path
    api_key_file: Path | None
    lane: str
    max_rounds: int
    context_window: int
    http_retries: int
    model_digests: dict[str, str]


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _model_digests(raw: str) -> dict[str, str]:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise ValueError("GILA_BENCH_MODEL_DIGESTS must be a JSON object") from error
    if not isinstance(value, dict) or not all(
        isinstance(key, str) and isinstance(digest, str)
        for key, digest in value.items()
    ):
        raise ValueError(
            "GILA_BENCH_MODEL_DIGESTS must map model IDs to SHA-256 strings"
        )
    for model, digest in value.items():
        if not _SHA256.fullmatch(digest):
            raise ValueError(f"invalid SHA-256 for {model!r}")
    return value


def _container_workdir(stdout: str) -> str:
    without_final_newline = stdout.removesuffix("\n")
    if "\n" in without_final_newline or "\r" in stdout:
        raise RuntimeError("Gila workdir probe must return exactly one path")
    workdir = without_final_newline
    if (
        not workdir
        or not workdir.isprintable()
        or not posixpath.isabs(workdir)
        or workdir.startswith("//")
        or posixpath.normpath(workdir) != workdir
    ):
        raise RuntimeError(
            f"Gila workdir probe returned a non-normalized absolute path: {workdir!r}"
        )
    return workdir


def _contract(
    trace: str,
    model: str,
    digest: str,
    *,
    agent_version: str,
    profile_sha256: str,
    context_window: int,
    max_rounds: int,
    workdir: str,
) -> dict[str, Any]:
    contracts: list[dict[str, Any]] = []
    solve_results: list[dict[str, Any]] = []
    for line in trace.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(value, dict):
            continue
        if "contract_version" in value:
            contracts.append(value)
        if value.get("kind") == "solve_result":
            solve_results.append(value)
    if len(contracts) != 1:
        raise RuntimeError(
            f"Gila trace has {len(contracts)} contract records; expected one"
        )
    if len(solve_results) != 1:
        raise RuntimeError(
            f"Gila trace has {len(solve_results)} solve_result records; expected one"
        )
    if solve_results[0].get("cwd") != workdir:
        raise RuntimeError(
            f"Gila solve_result cwd={solve_results[0].get('cwd')!r}; "
            f"expected {workdir!r}"
        )

    record = contracts[0]
    expected = {
        "contract_version": "1",
        "agent": "gilamonster-agent",
        "agent_version": agent_version,
        "requested_model": model,
        "effective_model": model,
        "profile_sha256": profile_sha256,
        "capabilities_manifest_sha256": _EMPTY_SHA256,
    }
    for field, wanted in expected.items():
        if record.get(field) != wanted:
            raise RuntimeError(
                f"Gila contract {field}={record.get(field)!r}; expected {wanted!r}"
            )
    if record.get("outcome") not in _OUTCOMES:
        raise RuntimeError(
            f"Gila contract has unknown outcome {record.get('outcome')!r}"
        )
    if record.get("model_digest") != digest:
        raise RuntimeError("Gila contract model digest does not match the campaign pin")
    if record.get("model_digest_source") != "operator_supplied":
        raise RuntimeError(
            "Gila contract must label its model digest as operator-supplied"
        )
    airframe = record.get("airframe")
    if airframe != {"name": "newt-agent", "revision": _AIRFRAME_REVISION}:
        raise RuntimeError("Gila contract airframe does not match the campaign pin")
    backend = record.get("backend")
    if not isinstance(backend, dict) or backend.get("kind") != "openai":
        raise RuntimeError("Gila contract backend must be OpenAI-compatible")

    effective = record.get("effective_config")
    expected_config = {
        "context_window": context_window,
        "max_rounds": max_rounds,
        "progress_grace_rounds": 5,
        "tenacity": "standard",
        "cognition": "default",
        "crew": "off",
        "ocap": "off",
        "tool_routing": "off",
        "self_verify": "off",
        "flight_recorder": "on",
        "gila_capabilities": [],
    }
    if not isinstance(effective, dict):
        raise RuntimeError("Gila contract has no effective configuration")
    for field, wanted in expected_config.items():
        if effective.get(field) != wanted:
            raise RuntimeError(
                f"Gila contract effective_config.{field}={effective.get(field)!r}; "
                f"expected {wanted!r}"
            )
    return record


class GilaAgent(BaseInstalledAgent):
    """Drive one native Gila turn inside a Harbor task container."""

    @staticmethod
    @override
    def name() -> str:
        return "gilamonster-agent"

    @override
    def get_version_command(self) -> str | None:
        return "gila --version"

    @override
    def parse_version(self, stdout: str) -> str:
        return stdout.strip().removeprefix("gila ")

    def _settings(self) -> BenchSettings:
        def value(name: str, default: str | None = None) -> str:
            resolved = self._get_env(name)
            if resolved is None:
                resolved = default
            if resolved is None or not resolved.strip():
                raise ValueError(f"{name} is required")
            return resolved

        try:
            max_rounds = int(value("GILA_BENCH_MAX_ROUNDS", "40"))
            context_window = int(value("GILA_BENCH_CONTEXT_WINDOW", "65536"))
            http_retries = int(value("GILA_BENCH_HTTP_RETRIES", "10"))
        except ValueError as error:
            raise ValueError(
                "Gila numeric benchmark settings must be integers"
            ) from error
        if min(max_rounds, context_window, http_retries) <= 0:
            raise ValueError("Gila numeric benchmark settings must be positive")
        api_key_value = self._get_env("GILA_BENCH_API_KEY_FILE")
        return BenchSettings(
            binary=Path(value("GILA_BENCH_BIN")).expanduser(),
            profile=Path(value("GILA_BENCH_PROFILE")).expanduser(),
            api_key_file=(Path(api_key_value).expanduser() if api_key_value else None),
            lane=value("GILA_BENCH_LANE"),
            max_rounds=max_rounds,
            context_window=context_window,
            http_retries=http_retries,
            model_digests=_model_digests(value("GILA_BENCH_MODEL_DIGESTS", "{}")),
        )

    def _preflight(self) -> tuple[BenchSettings, str, str]:
        settings = self._settings()
        if not settings.binary.is_file():
            raise ValueError("GILA_BENCH_BIN must name the portable Gila binary")
        if not settings.profile.is_file():
            raise ValueError("GILA_BENCH_PROFILE must name an exact backend profile")
        if settings.api_key_file is not None and not settings.api_key_file.is_file():
            raise ValueError("GILA_BENCH_API_KEY_FILE does not exist")
        if settings.lane != "yolo":
            raise ValueError("GILA_BENCH_LANE must be 'yolo' for this OCAP-off adapter")
        if self._parsed_model_provider != "gila" or not self._parsed_model_name:
            raise ValueError("Harbor model must be gila/<exact-served-model-id>")
        digest = settings.model_digests.get(self._parsed_model_name)
        if digest is None:
            raise ValueError(
                f"GILA_BENCH_MODEL_DIGESTS has no pin for {self._parsed_model_name!r}"
            )
        return settings, self._parsed_model_name, digest

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        settings, _, _ = self._preflight()
        binary_digest = _file_sha256(settings.binary)
        profile_digest = _file_sha256(settings.profile)
        adapter_digest = _file_sha256(Path(__file__).resolve())
        await self.exec_as_root(environment, command="mkdir -p /etc/gila")
        await environment.upload_file(settings.binary, "/usr/local/bin/gila")
        await environment.upload_file(settings.profile, "/etc/gila/bench.toml")
        with tempfile.NamedTemporaryFile("w", delete=False) as handle:
            empty_manifest = Path(handle.name)
        try:
            await environment.upload_file(empty_manifest, "/etc/gila/capabilities.toml")
        finally:
            empty_manifest.unlink(missing_ok=True)
        if settings.api_key_file is not None:
            await environment.upload_file(settings.api_key_file, "/etc/gila/api-key")
        result = await self.exec_as_root(
            environment,
            command=("chmod 0755 /usr/local/bin/gila && sha256sum /usr/local/bin/gila"),
        )
        installed_digest = result.stdout.strip().split()[0]
        if installed_digest != binary_digest:
            raise RuntimeError(
                "installed Gila binary digest differs from the host artifact"
            )
        private_paths = ["/etc/gila/bench.toml", "/etc/gila/capabilities.toml"]
        if settings.api_key_file is not None:
            private_paths.append("/etc/gila/api-key")
        await self.exec_as_root(
            environment,
            command="chmod 0600 " + " ".join(private_paths),
        )
        agent_user = shlex.quote(str(environment.default_user or "root"))
        await self.exec_as_root(
            environment,
            command=f"chown {agent_user} " + " ".join(private_paths),
        )
        version_result = await self.exec_as_agent(
            environment, command="/usr/local/bin/gila --version"
        )
        version = self.parse_version(version_result.stdout)
        if not _VERSION.fullmatch(version):
            raise RuntimeError(f"non-publishable Gila build identity {version!r}")
        workdir_result = await self.exec_as_agent(environment, "pwd -P")
        workdir = _container_workdir(workdir_result.stdout)
        self._version = version
        self._installed_settings = settings
        self._binary_sha256 = binary_digest
        self._profile_sha256 = profile_digest
        self._adapter_sha256 = adapter_digest
        self._workdir = workdir

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        settings, model, digest = self._preflight()
        if getattr(self, "_installed_settings", None) != settings:
            raise RuntimeError("Gila benchmark settings changed after installation")
        if _file_sha256(Path(__file__).resolve()) != self._adapter_sha256:
            raise RuntimeError("Gila Harbor adapter changed after installation")
        agent_version = self.version()
        if agent_version is None:
            raise RuntimeError(
                "Gila build identity was not established during installation"
            )
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as handle:
            handle.write(instruction)
            instruction_path = Path(handle.name)
        try:
            await environment.upload_file(instruction_path, "/tmp/gila-task.md")
        finally:
            instruction_path.unlink(missing_ok=True)

        argv = [
            "/usr/local/bin/gila",
            "solve",
            "--cwd",
            self._workdir,
            "--instruction-file",
            "/tmp/gila-task.md",
            "--config",
            "/etc/gila/bench.toml",
            "--model",
            model,
            "--events",
            "/logs/agent/gila-events.jsonl",
            "--max-rounds",
            str(settings.max_rounds),
            "--context-window",
            str(settings.context_window),
            "--unsafe-host-exec",
        ]
        argv.extend(["--model-digest", digest])
        env = {
            "GILA_CAPABILITIES_MANIFEST": "/etc/gila/capabilities.toml",
            "NEWT_PROVIDER": "",
            "NEWT_HTTP_MAX_RETRIES": str(settings.http_retries),
            "NEWT_HTTP_BACKOFF_BASE_MS": "2000",
            "NEWT_HTTP_BACKOFF_MAX_MS": "30000",
            "NEWT_SELF_VERIFY": "0",
            "NEWT_FLIGHT_RECORDER": "/logs/agent/gila-flight-recorder.jsonl",
        }

        process_error: Exception | None = None
        try:
            await self.exec_as_agent(
                environment,
                command=(
                    "mkdir -p /logs/agent && "
                    "rm -f /logs/agent/gila-events.jsonl "
                    "/logs/agent/gila-flight-recorder.jsonl && " + shlex.join(argv)
                ),
                env=env,
                cwd=self._workdir,
            )
        except Exception as error:  # validate typed failure contracts too
            process_error = error

        with tempfile.NamedTemporaryFile("r", delete=False) as handle:
            trace_path = Path(handle.name)
        trace_error: Exception | None = None
        record: dict[str, Any] | None = None
        try:
            await environment.download_file("/logs/agent/gila-events.jsonl", trace_path)
            record = _contract(
                trace_path.read_text(),
                model,
                digest,
                agent_version=agent_version,
                profile_sha256=self._profile_sha256,
                context_window=settings.context_window,
                max_rounds=settings.max_rounds,
                workdir=self._workdir,
            )
        except Exception as error:
            trace_error = error
        finally:
            trace_path.unlink(missing_ok=True)
        if trace_error is not None:
            if process_error is not None:
                raise trace_error from process_error
            raise trace_error
        assert record is not None

        timing = record.get("timing") or {}
        if isinstance(timing.get("gen_tokens"), int):
            context.n_output_tokens = timing["gen_tokens"]
        context.metadata = {
            "gila_contract": record,
            "gila_adapter_sha256": self._adapter_sha256,
            "gila_binary_sha256": self._binary_sha256,
            "gila_profile_sha256": self._profile_sha256,
            "gila_workdir": self._workdir,
        }
        if process_error is not None:
            raise process_error
