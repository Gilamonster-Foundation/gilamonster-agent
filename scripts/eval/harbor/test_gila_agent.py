import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

from gila_agent import GilaAgent, _contract
from harbor.models.agent.context import AgentContext


MODEL = "qwen3.6_35b"
DIGEST = "a" * 64
VERSION = "0.4.0 (123456789abc)"
PROFILE_SHA256 = "b" * 64
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


def record(**overrides):
    value = {
        "contract_version": "1",
        "agent": "gilamonster-agent",
        "agent_version": VERSION,
        "requested_model": MODEL,
        "effective_model": MODEL,
        "model_digest": DIGEST,
        "model_digest_source": "operator_supplied",
        "outcome": "completed",
        "airframe": {
            "name": "newt-agent",
            "revision": "8cde29f0b16cab9206fb76d94925b1ea49ee68bc",
        },
        "backend": {"name": "dgx1", "kind": "openai"},
        "profile_sha256": PROFILE_SHA256,
        "capabilities_manifest_sha256": EMPTY_SHA256,
        "effective_config": {
            "context_window": 65536,
            "max_rounds": 40,
            "progress_grace_rounds": 5,
            "tenacity": "standard",
            "cognition": "default",
            "crew": "off",
            "ocap": "off",
            "tool_routing": "off",
            "self_verify": "off",
            "flight_recorder": "on",
            "gila_capabilities": [],
        },
    }
    value.update(overrides)
    return value


def parse(trace):
    return _contract(
        trace,
        MODEL,
        DIGEST,
        agent_version=VERSION,
        profile_sha256=PROFILE_SHA256,
        context_window=65536,
        max_rounds=40,
    )


class ContractTests(unittest.TestCase):
    def test_accepts_one_native_gila_contract(self):
        value = record()
        self.assertEqual(parse(json.dumps(value)), value)

    def test_rejects_relabelled_newt_contract(self):
        with self.assertRaisesRegex(RuntimeError, "agent"):
            parse(json.dumps(record(agent="newt-agent")))

    def test_rejects_configuration_drift(self):
        drifted = record()
        drifted["effective_config"]["max_rounds"] = 39
        with self.assertRaisesRegex(RuntimeError, "max_rounds"):
            parse(json.dumps(drifted))

    def test_rejects_ambiguous_trace(self):
        line = json.dumps(record())
        with self.assertRaisesRegex(RuntimeError, "2 contract records"):
            parse(f"{line}\n{line}\n")


class InstallTests(unittest.IsolatedAsyncioTestCase):
    async def test_install_uses_extra_env_chowns_and_executes_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "gila"
            profile = root / "bench.toml"
            binary.write_bytes(b"portable-gila")
            profile.write_text("profile")
            binary_sha256 = hashlib.sha256(binary.read_bytes()).hexdigest()
            agent = GilaAgent(
                logs_dir=root / "logs",
                model_name=f"gila/{MODEL}",
                extra_env={
                    "GILA_BENCH_BIN": str(binary),
                    "GILA_BENCH_PROFILE": str(profile),
                    "GILA_BENCH_LANE": "yolo",
                    "GILA_BENCH_MODEL_DIGESTS": json.dumps({MODEL: DIGEST}),
                },
            )
            environment = SimpleNamespace(
                default_user="bench",
                upload_file=AsyncMock(),
            )

            async def root_exec(*_args, **kwargs):
                stdout = ""
                if "sha256sum" in kwargs["command"]:
                    stdout = f"{binary_sha256}  /usr/local/bin/gila\n"
                return SimpleNamespace(stdout=stdout)

            agent.exec_as_root = AsyncMock(side_effect=root_exec)
            agent.exec_as_agent = AsyncMock(
                return_value=SimpleNamespace(stdout=f"gila {VERSION}\n")
            )

            await agent.install(environment)

            self.assertEqual(agent.version(), VERSION)
            self.assertEqual(
                agent._adapter_sha256,
                hashlib.sha256(
                    Path(__file__).with_name("gila_agent.py").read_bytes()
                ).hexdigest(),
            )
            self.assertEqual(environment.upload_file.await_count, 3)
            root_commands = [
                call.kwargs["command"] for call in agent.exec_as_root.await_args_list
            ]
            self.assertTrue(any(command.startswith("chown bench ") for command in root_commands))
            agent.exec_as_agent.assert_awaited_once_with(
                environment, command="/usr/local/bin/gila --version"
            )

    async def test_run_records_loaded_adapter_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "gila"
            profile = root / "bench.toml"
            binary.write_bytes(b"portable-gila")
            profile.write_text("profile")
            agent = GilaAgent(
                logs_dir=root / "logs",
                model_name=f"gila/{MODEL}",
                extra_env={
                    "GILA_BENCH_BIN": str(binary),
                    "GILA_BENCH_PROFILE": str(profile),
                    "GILA_BENCH_LANE": "yolo",
                    "GILA_BENCH_MODEL_DIGESTS": json.dumps({MODEL: DIGEST}),
                },
            )
            agent._installed_settings = agent._settings()
            agent._version = VERSION
            agent._binary_sha256 = hashlib.sha256(binary.read_bytes()).hexdigest()
            agent._profile_sha256 = hashlib.sha256(profile.read_bytes()).hexdigest()
            agent._adapter_sha256 = hashlib.sha256(
                Path(__file__).with_name("gila_agent.py").read_bytes()
            ).hexdigest()

            async def download_trace(_remote_path, local_path):
                Path(local_path).write_text(
                    json.dumps(
                        record(
                            profile_sha256=agent._profile_sha256,
                            timing={"gen_tokens": 7},
                        )
                    )
                )

            environment = SimpleNamespace(
                upload_file=AsyncMock(),
                download_file=AsyncMock(side_effect=download_trace),
            )
            agent.exec_as_agent = AsyncMock(return_value=SimpleNamespace(stdout=""))
            context = AgentContext()

            await agent.run("task", environment, context)

            self.assertEqual(
                context.metadata["gila_adapter_sha256"], agent._adapter_sha256
            )
            self.assertEqual(context.n_output_tokens, 7)

    async def test_run_rejects_missing_contract_after_process_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "gila"
            profile = root / "bench.toml"
            binary.write_bytes(b"portable-gila")
            profile.write_text("profile")
            agent = GilaAgent(
                logs_dir=root / "logs",
                model_name=f"gila/{MODEL}",
                extra_env={
                    "GILA_BENCH_BIN": str(binary),
                    "GILA_BENCH_PROFILE": str(profile),
                    "GILA_BENCH_LANE": "yolo",
                    "GILA_BENCH_MODEL_DIGESTS": json.dumps({MODEL: DIGEST}),
                },
            )
            agent._installed_settings = agent._settings()
            agent._version = VERSION
            agent._binary_sha256 = hashlib.sha256(binary.read_bytes()).hexdigest()
            agent._profile_sha256 = hashlib.sha256(profile.read_bytes()).hexdigest()
            agent._adapter_sha256 = hashlib.sha256(
                Path(__file__).with_name("gila_agent.py").read_bytes()
            ).hexdigest()
            environment = SimpleNamespace(
                upload_file=AsyncMock(),
                download_file=AsyncMock(side_effect=FileNotFoundError("no trace")),
            )
            process_error = RuntimeError("candidate failed")
            agent.exec_as_agent = AsyncMock(side_effect=process_error)

            with self.assertRaises(FileNotFoundError) as caught:
                await agent.run("task", environment, AgentContext())

            self.assertIs(caught.exception.__cause__, process_error)
            command = agent.exec_as_agent.await_args.kwargs["command"]
            self.assertIn("/usr/local/bin/gila solve", command)


if __name__ == "__main__":
    unittest.main()
