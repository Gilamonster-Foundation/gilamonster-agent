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


def solve_result(workdir="/app"):
    return {"kind": "solve_result", "cwd": workdir}


def trace(*records):
    return "\n".join(json.dumps(value) for value in records) + "\n"


def parse(value, workdir="/app"):
    return _contract(
        value,
        MODEL,
        DIGEST,
        agent_version=VERSION,
        profile_sha256=PROFILE_SHA256,
        context_window=65536,
        max_rounds=40,
        workdir=workdir,
    )


def agent_fixture(root):
    binary = root / "gila"
    profile = root / "bench.toml"
    binary.write_bytes(b"portable-gila")
    profile.write_text("profile")
    return GilaAgent(
        logs_dir=root / "logs",
        model_name=f"gila/{MODEL}",
        extra_env={
            "GILA_BENCH_BIN": str(binary),
            "GILA_BENCH_PROFILE": str(profile),
            "GILA_BENCH_LANE": "yolo",
            "GILA_BENCH_MODEL_DIGESTS": json.dumps({MODEL: DIGEST}),
        },
    )


class FakeEnvironment:
    def __init__(self, binary_sha256, workdir_output):
        self.default_user = "bench"
        self.binary_sha256 = binary_sha256
        self.workdir_output = workdir_output
        self.exec_calls = []
        self.uploads = []
        self.trace = ""

    async def exec(
        self,
        command,
        cwd=None,
        env=None,
        timeout_sec=None,
        user=None,
    ):
        self.exec_calls.append(
            SimpleNamespace(
                command=command,
                cwd=cwd,
                env=env,
                timeout_sec=timeout_sec,
                user=user,
            )
        )
        stdout = ""
        if "sha256sum /usr/local/bin/gila" in command:
            stdout = f"{self.binary_sha256}  /usr/local/bin/gila\n"
        elif command.endswith("/usr/local/bin/gila --version"):
            stdout = f"gila {VERSION}\n"
        elif command.endswith("pwd -P"):
            stdout = self.workdir_output
        return SimpleNamespace(return_code=0, stdout=stdout, stderr="")

    async def upload_file(self, source_path, target_path):
        self.uploads.append((Path(source_path), target_path))

    async def download_file(self, _source_path, target_path):
        Path(target_path).write_text(self.trace)


class ContractTests(unittest.TestCase):
    def test_accepts_one_native_gila_contract(self):
        value = record()
        self.assertEqual(parse(trace(solve_result(), value)), value)

    def test_rejects_relabelled_newt_contract(self):
        with self.assertRaisesRegex(RuntimeError, "agent"):
            parse(trace(solve_result(), record(agent="newt-agent")))

    def test_rejects_configuration_drift(self):
        drifted = record()
        drifted["effective_config"]["max_rounds"] = 39
        with self.assertRaisesRegex(RuntimeError, "max_rounds"):
            parse(trace(solve_result(), drifted))

    def test_rejects_ambiguous_trace(self):
        with self.assertRaisesRegex(RuntimeError, "2 contract records"):
            parse(trace(solve_result(), record(), record()))

    def test_rejects_solve_result_workdir_drift(self):
        with self.assertRaisesRegex(RuntimeError, "solve_result cwd"):
            parse(trace(solve_result("/workspace"), record()), workdir="/app")


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

            async def agent_exec(*_args, **kwargs):
                command = kwargs["command"] if "command" in kwargs else _args[1]
                if command == "/usr/local/bin/gila --version":
                    return SimpleNamespace(stdout=f"gila {VERSION}\n")
                if command == "pwd -P":
                    return SimpleNamespace(stdout="/app\n")
                raise AssertionError(f"unexpected agent command: {command}")

            agent.exec_as_agent = AsyncMock(side_effect=agent_exec)

            await agent.install(environment)

            self.assertEqual(agent.version(), VERSION)
            self.assertEqual(
                agent._adapter_sha256,
                hashlib.sha256(
                    Path(__file__).with_name("gila_agent.py").read_bytes()
                ).hexdigest(),
            )
            self.assertEqual(agent._workdir, "/app")
            self.assertEqual(environment.upload_file.await_count, 3)
            root_commands = [
                call.kwargs["command"] for call in agent.exec_as_root.await_args_list
            ]
            self.assertTrue(
                any(command.startswith("chown bench ") for command in root_commands)
            )
            agent.exec_as_agent.assert_any_await(
                environment, command="/usr/local/bin/gila --version"
            )
            agent.exec_as_agent.assert_any_await(environment, "pwd -P")

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
            agent._workdir = "/workspace"

            async def download_trace(_remote_path, local_path):
                Path(local_path).write_text(
                    trace(
                        solve_result("/workspace"),
                        record(
                            profile_sha256=agent._profile_sha256,
                            timing={"gen_tokens": 7},
                        ),
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
            self.assertEqual(context.metadata["gila_workdir"], "/workspace")
            self.assertEqual(agent.exec_as_agent.await_args.kwargs["cwd"], "/workspace")
            self.assertIn(
                "--cwd /workspace", agent.exec_as_agent.await_args.kwargs["command"]
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
            agent._workdir = "/app"
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

    async def test_fake_environments_bind_each_task_workdir(self):
        for workdir in ("/app", "/workspace", "/app/personal-site"):
            with (
                self.subTest(workdir=workdir),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                agent = agent_fixture(root)
                binary_sha256 = hashlib.sha256((root / "gila").read_bytes()).hexdigest()
                environment = FakeEnvironment(binary_sha256, f"{workdir}\n")

                await agent.install(environment)
                environment.trace = trace(
                    solve_result(workdir),
                    record(profile_sha256=agent._profile_sha256),
                )
                context = AgentContext()
                await agent.run("task", environment, context)

                probe = [
                    call
                    for call in environment.exec_calls
                    if call.command.endswith("pwd -P")
                ]
                self.assertEqual(len(probe), 1)
                self.assertIsNone(probe[0].cwd)
                solve = [
                    call
                    for call in environment.exec_calls
                    if "/usr/local/bin/gila solve" in call.command
                ]
                self.assertEqual(len(solve), 1)
                self.assertEqual(solve[0].cwd, workdir)
                self.assertIn(f"--cwd {workdir}", solve[0].command)
                self.assertEqual(context.metadata["gila_workdir"], workdir)

    async def test_install_rejects_invalid_workdir_probe_output(self):
        invalid_outputs = (
            "",
            "app\n",
            "/app/../workspace\n",
            "/app/\n",
            "//app\n",
            "/app\n/workspace\n",
            "/app\x00\n",
            "/app\v",
        )
        for output in invalid_outputs:
            with (
                self.subTest(output=output),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                agent = agent_fixture(root)
                binary_sha256 = hashlib.sha256((root / "gila").read_bytes()).hexdigest()
                environment = FakeEnvironment(binary_sha256, output)

                with self.assertRaisesRegex(RuntimeError, "workdir probe"):
                    await agent.install(environment)


if __name__ == "__main__":
    unittest.main()
