#!/usr/bin/env python3
"""scripts/kit-acp-smoke.py against a fake `docker` whose containers are local
fake ACP adapters (good, Friday's codex session/new error, a prompt that never
returns, ...), talking to the script's real fake model endpoint."""
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import socket
import textwrap
import time
import unittest

try:
    import yaml
except ImportError:  # the system interpreter; make test-tools also runs the publisher venv
    yaml = None

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/kit-acp-smoke.py"

FAKE_DOCKER = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import json, os, sys
    images = json.load(open(os.environ["FAKE_IMAGES"]))
    args = sys.argv[1:]
    def log(entry):
        with open(os.environ["FAKE_LOG"], "a") as stream:
            stream.write(json.dumps(entry) + "\\n")
    if args[:3] == ["buildx", "imagetools", "inspect"]:
        log({"command": args})
        image = images.get(args[-1])
        if image is None or image.get("registry") is False:
            print(f"ERROR: {args[-1]}: not found", file=sys.stderr)
            sys.exit(1)
        annotations = {"vnd.docker.sandbox.kit.descriptor": json.dumps(image["descriptor"])} if "descriptor" in image else {}
        print(json.dumps({"annotations": annotations}))
    elif args[:2] == ["network", "inspect"]:
        print("127.0.0.1")
    elif args[:2] == ["image", "inspect"]:
        if "--format" in args:
            print(json.dumps(["PATH=/usr/local/bin:/usr/bin:/bin", "OTHER=1"]))
    elif args[0] == "pull":
        log({"command": args})
    elif args[0] == "kill":
        log({"command": args})
        pidfile = os.path.join(os.path.dirname(os.environ["FAKE_LOG"]), args[1] + ".pid")
        if os.path.exists(pidfile):
            os.kill(int(open(pidfile).read()), 9)
    elif args[0] == "run":
        booleans = {"--rm", "--interactive", "--init"}
        env, mounts, hosts, at = {}, {}, [], 1
        while args[at].startswith("--"):
            if args[at] in booleans:
                at += 1
                continue
            flag, value = args[at], args[at + 1]
            if flag == "--env":
                key, _, value = value.partition("=")
                env[key] = value
            elif flag == "--mount":
                fields = dict(part.split("=", 1) if "=" in part else (part, "") for part in value.split(","))
                mounts[fields["target"]] = fields
            elif flag == "--entrypoint":
                entrypoint = value
            elif flag == "--add-host":
                hosts.append(value)
            at += 2
        image, rest = args[at], args[at + 1:]
        log({"command": args, "image": image, "entrypoint": [entrypoint, *rest], "env": env, "mounts": mounts, "hosts": hosts})
        name = args[args.index("--name") + 1]
        with open(os.path.join(os.path.dirname(os.environ["FAKE_LOG"]), name + ".pid"), "w") as stream:
            stream.write(str(os.getpid()))
        os.environ.update({key: value.replace("host.docker.internal", "127.0.0.1") for key, value in env.items()})
        os.environ["FAKE_RUN"] = mounts["/run/marsh"]["source"]
        os.environ["FAKE_HOME"] = mounts[env["HOME"]]["source"]
        os.environ["FAKE_BEHAVIOUR"] = images[image]["behaviour"]
        os.execv(sys.executable, [sys.executable, os.environ["FAKE_AGENT"]])
    else:
        sys.exit(f"fake docker: {args}")
    """
)

FAKE_AGENT = textwrap.dedent(
    """\
    import json, os, pathlib, subprocess, sys, urllib.request
    behaviour = os.environ["FAKE_BEHAVIOUR"]
    run = pathlib.Path(os.environ["FAKE_RUN"])
    # The job's capability, as the Kits read it.
    assert json.loads((run / "job.json").read_text())["socket"] == "/run/marsh/cap.sock"
    assert (run / "context.md").read_text().strip()
    assert subprocess.run(["sh", str(run / "bin/marsh"), "task-mcp", "--help"], capture_output=True).returncode == 0
    assert os.environ["PATH"].startswith("/run/marsh/bin:")
    def send(message):
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", **message}) + "\\n")
        sys.stdout.flush()
    def update(value):
        send({"method": "session/update", "params": {"sessionId": "s1", "update": value}})
    key = "real-key" if behaviour == "real-key" else "proxy-managed"
    def post(url, body, headers):
        url = url.replace("host.docker.internal", "127.0.0.1").replace("mcp-gateway.docker.internal", "127.0.0.1")
        request = urllib.request.Request(url, json.dumps(body).encode(), {"content-type": "application/json", **headers})
        return urllib.request.urlopen(request, timeout=10).read().decode()
    def gateway():
        # As the Kits' proxies do: the Gateway with the sentinel's name as bearer.
        port = os.environ["HTTP_PROXY"].rsplit(":", 1)[1]
        token = "wrong" if behaviour == "gateway-bad-token" else os.environ["MCP_SENTINEL_TOKEN_NAME"]
        assert os.environ["MCP_GATEWAY_URL"] == "http://mcp-gateway.docker.internal/mcp"
        post(f"http://127.0.0.1:{port}/mcp", {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
             {"authorization": f"Bearer {token}"})
    def model():
        headers = {}
        if "ANTHROPIC_BASE_URL" in os.environ:
            url, body = os.environ["ANTHROPIC_BASE_URL"] + "/v1/messages", {"stream": True}
            headers = {"x-api-key": key}
        elif "CODEX_CONFIG" in os.environ:
            url, body = json.loads(os.environ["CODEX_CONFIG"])["openai_base_url"] + "/responses", {"stream": True}
            headers = {"authorization": f"Bearer {key}"}
        else:
            home = pathlib.Path(os.environ["FAKE_HOME"])
            [models] = home.glob(".pi*/agent/models.json")
            providers = json.loads(models.read_text())["providers"]
            url, body = providers["marsh-smoke"]["baseUrl"] + "/chat/completions", {"stream": True}
        text = post(url, body, headers)
        return "ok" if '"ok"' in text else "?"
    for line in sys.stdin:
        message = json.loads(line)
        method, ident = message.get("method"), message.get("id")
        if method == "initialize":
            send({"id": ident, "result": {"protocolVersion": 2 if behaviour == "version2" else 1,
                                          "agentCapabilities": {}}})
        elif method == "session/new":
            if behaviour in ("gateway", "gateway-bad-token"):
                gateway()
            if behaviour == "broken-session" or (behaviour == "broken-as-in-a-job" and "CODEX_CONFIG" not in os.environ):
                send({"id": ident, "error": {"code": -32603, "message": "Internal error", "data": {
                    "details": "failed to load configuration: invalid transport in `mcp_servers.marsh`"}}})
            elif behaviour == "close-stdout":
                import time
                sys.stdout.close()
                os.close(1)
                time.sleep(600)
            elif behaviour == "exit":
                print("adapter crashed", file=sys.stderr)
                sys.exit(3)
            else:
                send({"id": ident, "result": {"sessionId": "s1"}})
        elif method == "session/prompt":
            if behaviour == "no-call":
                update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "ok"}})
                send({"id": ident, "result": {"stopReason": "end_turn"}})
                continue
            if behaviour == "permission":
                send({"id": 99, "method": "session/request_permission", "params": {}})
            text = model()
            update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
            update({"sessionUpdate": "usage_update", "used": 6, "size": 1000})
            if behaviour == "hang-prompt":
                continue  # pi's report: the chunk and usage arrive, the result never does
            send({"id": ident, "result": {"stopReason": "refusal" if behaviour == "refusal" else "end_turn"}})
    """
)


def descriptor(agent, command=None):
    capabilities = [{"type": "com.docker.sandbox/sbx@1"}]
    if agent:
        capabilities.append({"type": "com.docker.sandbox/acp@1",
                             "config": {"agent": agent, "command": command or [f"/usr/local/bin/marsh-{agent}", "--acp"]}})
    return {"schemaVersion": "3", "capabilities": capabilities}


class KitAcpSmoke(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="kit-acp-smoke-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        self.docker = self.root / "docker"
        self.docker.write_text(FAKE_DOCKER)
        self.docker.chmod(0o755)
        (self.root / "agent.py").write_text(FAKE_AGENT)
        self.log = self.root / "log.jsonl"
        self.images = {}

    def image(self, reference, agent, behaviour="good", **extra):
        self.images[reference] = {"descriptor": descriptor(agent, **extra), "behaviour": behaviour}

    def calls(self, command):
        lines = self.log.read_text().splitlines() if self.log.exists() else []
        return [entry for entry in map(json.loads, lines) if entry["command"][:len(command)] == command]

    def smoke(self, *arguments):
        (self.root / "images.json").write_text(json.dumps(self.images))
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        environment = {**os.environ, "FAKE_IMAGES": str(self.root / "images.json"), "FAKE_LOG": str(self.log),
                       "FAKE_AGENT": str(self.root / "agent.py"), "MARSH_SMOKE_PROXY_PORT": str(port),
                       "MARSH_SMOKE_RETRY_PAUSES": "0 0"}
        return subprocess.run([sys.executable, str(SCRIPT), "--docker", str(self.docker), "--start-timeout", "20",
                               "--session-timeout", "20", "--prompt-timeout", "20", *arguments],
                              capture_output=True, text=True, timeout=120, env=environment)

    def runs(self):
        lines = self.log.read_text().splitlines() if self.log.exists() else []
        return [entry for entry in map(json.loads, lines) if entry["command"][0] == "run"]

    def test_every_agent_kit_passes_initialize_session_new_and_a_prompt_against_the_fake_endpoint(self):
        self.image("reg/claude@sha256:1", "claude")
        self.image("reg/codex@sha256:2", "codex")
        self.image("reg/pi@sha256:3", "pi")
        self.image("reg/pix@sha256:4", "pix", command=["/usr/local/bin/marsh-pix", "--acp"])
        commands = self.root / "commands.json"
        commands.write_text(json.dumps({"claude": "reg/claude@sha256:1", "codex": "reg/codex@sha256:2",
                                        "pi": "reg/pi@sha256:3", "pix": "reg/pix@sha256:4"}))
        result = self.smoke("--commands", str(commands))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("ok claude: initialize, session/new, session/prompt (end_turn, x-api-key sentinel)",
                      result.stdout)
        self.assertIn("ok codex: initialize and session/new without CODEX_CONFIG, initialize, session/new, "
                      "session/prompt (end_turn, authorization sentinel)", result.stdout)
        for name in ("pi", "pix"):
            self.assertIn(f"ok {name}: initialize, session/new, session/prompt (end_turn)", result.stdout)

    def test_the_adapter_runs_the_way_a_marsh_acp_job_runs_it(self):
        self.image("reg/codex@sha256:2", "codex", command=["/usr/local/bin/marsh-codex", "--acp", "--x"])
        result = self.smoke("codex=reg/codex@sha256:2")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        plain, run = self.runs()
        self.assertNotIn("CODEX_CONFIG", plain["env"])
        self.assertIn("openai_base_url", json.loads(run["env"]["CODEX_CONFIG"]))
        self.assertEqual(run["entrypoint"], ["/usr/local/bin/marsh-codex", "--acp", "--x"])
        self.assertEqual(run["mounts"]["/run/marsh"].get("readonly"), "")
        command = run["command"]
        for flag, value in (("--user", f"{os.getuid()}:{os.getgid()}"), ("--cap-drop", "ALL"),
                            ("--security-opt", "no-new-privileges=true")):
            self.assertEqual(command[command.index(flag) + 1], value)
        self.assertIn("--init", command)
        env = run["env"]
        self.assertEqual(env["PATH"], "/run/marsh/bin:/usr/local/bin:/usr/bin:/bin")
        self.assertEqual(env["HOME"], env["MARSH_SELECTED_HOME"])
        self.assertEqual(env["SBX_CRED_OPENAI_MODE"], "apikey")
        # The stock SBX network shape of a job (marsh-runtime PROXY_VARIABLES).
        self.assertEqual(env["MCP_GATEWAY_URL"], "http://mcp-gateway.docker.internal/mcp")
        self.assertRegex(env["MCP_SENTINEL_TOKEN_NAME"], r"^[A-Za-z0-9._-]{1,128}$")
        port = env["HTTP_PROXY"].rsplit(":", 1)[1]
        self.assertEqual(env["HTTP_PROXY"], f"http://gateway.docker.internal:{port}")
        self.assertEqual(env["HTTPS_PROXY"], env["HTTP_PROXY"])
        self.assertEqual(env["NO_PROXY"], "localhost,127.0.0.1,::1")
        for host in ("gateway.docker.internal", "mcp-gateway.docker.internal", "host.docker.internal"):
            self.assertIn(f"{host}:host-gateway", run["hosts"])

    def test_fridays_codex_session_new_error_fails_the_gate_and_the_rest_are_still_checked(self):
        self.image("reg/codex@sha256:bad", "codex", "broken-session")
        self.image("reg/pi@sha256:3", "pi")
        result = self.smoke("codex=reg/codex@sha256:bad", "pi=reg/pi@sha256:3")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAILED codex: session/new: error", result.stdout)
        self.assertIn("invalid transport in `mcp_servers.marsh`", result.stdout)
        self.assertIn("ok pi:", result.stdout)

    def test_a_prompt_whose_result_never_arrives_fails_after_the_prompt_timeout(self):
        self.image("reg/pi@sha256:hang", "pi", "hang-prompt")
        result = self.smoke("--prompt-timeout", "3", "pi=reg/pi@sha256:hang")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAILED pi: session/prompt: no result within 3s", result.stdout)

    def test_a_turn_must_end_with_end_turn_after_calling_the_model_and_show_its_reply(self):
        self.image("reg/a@sha256:1", "claude", "no-call")
        self.image("reg/b@sha256:2", "claude", "refusal")
        result = self.smoke("a=reg/a@sha256:1", "b=reg/b@sha256:2")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAILED a: session/prompt: ended without calling the model endpoint", result.stdout)
        self.assertIn("FAILED b: session/prompt: stopReason 'refusal', not 'end_turn'", result.stdout)

    def test_an_adapter_that_exits_fails_with_its_stderr(self):
        self.image("reg/a@sha256:1", "claude", "exit")
        result = self.smoke("a=reg/a@sha256:1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("session/new: the adapter exited (3) before its result", result.stdout)
        self.assertIn("adapter crashed", result.stdout)

    def test_an_adapter_that_is_not_acp_v1_fails(self):
        self.image("reg/a@sha256:1", "claude", "version2")
        result = self.smoke("a=reg/a@sha256:1")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAILED a: initialize: protocolVersion 2, not 1", result.stdout)
        self.assertEqual(len(self.runs()), 1)

    def test_an_adapter_that_closes_stdout_but_lives_on_fails_within_the_deadline_and_is_killed(self):
        self.image("reg/a@sha256:1", "claude", "close-stdout")
        started = time.monotonic()
        result = self.smoke("--session-timeout", "3", "a=reg/a@sha256:1")
        elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAILED a: session/new: the adapter closed its stdout before its result "
                      "and is still running", result.stdout)
        # The 3 s deadline, then close(): stdin closed, a short wait, docker kill.
        self.assertLess(elapsed, 40)
        kills = [entry["command"] for entry in map(json.loads, self.log.read_text().splitlines())
                 if entry["command"][0] == "kill"]
        self.assertEqual(len(kills), 1)
        self.assertTrue(kills[0][1].startswith("marsh-acp-smoke-a-"))
        pid = int((self.root / f"{kills[0][1]}.pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_agent_to_client_requests_are_refused_and_the_turn_still_ends(self):
        self.image("reg/a@sha256:1", "claude", "permission")
        result = self.smoke("a=reg/a@sha256:1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_no_prompt_checks_initialize_and_session_new_only(self):
        self.image("reg/pi@sha256:hang", "pi", "hang-prompt")
        result = self.smoke("--no-prompt", "pi=reg/pi@sha256:hang")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("ok pi: initialize, session/new (", result.stdout)

    def test_kits_without_an_adapter_are_skipped_and_unknown_agents_get_no_prompt(self):
        self.image("reg/shell@sha256:1", None)
        self.image("reg/other@sha256:2", "other", "hang-prompt")
        result = self.smoke("shell=reg/shell@sha256:1", "other=reg/other@sha256:2")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("skipped shell: its Kit declares no ACP adapter", result.stdout)
        self.assertIn("ok other: initialize, session/new, no prompt (no fake-endpoint setup for agent 'other')",
                      result.stdout)
        self.assertEqual(len(self.runs()), 1)

    def test_registry_errors_are_retried_and_reported_as_registry_errors(self):
        self.images["local/x:1"] = {"behaviour": "good", "registry": False}
        self.images["reg/y@sha256:1"] = {"behaviour": "good"}
        result = self.smoke("x=local/x:1", "y=reg/y@sha256:1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("FAILED x: registry: reading local/x:1 failed after 3 tries: ERROR: local/x:1: not found",
                      result.stdout)
        self.assertEqual(len(self.calls(["buildx", "imagetools", "inspect", "--raw", "local/x:1"])), 3)
        self.assertIn("FAILED y: the image's manifest has no Kit descriptor annotation; give --sources",
                      result.stdout)

    def test_the_mcp_gateway_is_called_with_the_sentinel_name(self):
        self.image("reg/a@sha256:1", "claude", "gateway")
        self.image("reg/b@sha256:2", "claude", "gateway-bad-token")
        result = self.smoke("a=reg/a@sha256:1", "b=reg/b@sha256:2")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertRegex(result.stdout, r"ok a: .*MCP Gateway reached \(1 requests\)")
        self.assertIn("FAILED b: the MCP Gateway was called with authorization Bearer wrong", result.stdout)

    def test_a_model_call_without_the_apikey_sentinel_fails(self):
        self.image("reg/claude@sha256:1", "claude", "real-key")
        self.image("reg/codex@sha256:2", "codex", "real-key")
        result = self.smoke("claude=reg/claude@sha256:1", "codex=reg/codex@sha256:2")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAILED claude: session/prompt: the model call's x-api-key was real-key, not the apikey-mode "
                      "sentinel 'proxy-managed'", result.stdout)
        self.assertIn("FAILED codex: session/prompt: the model call's authorization was Bearer real-key, not the "
                      "apikey-mode sentinel 'Bearer proxy-managed'", result.stdout)

    def test_codex_must_also_start_as_a_job_starts_it_without_codex_config(self):
        self.image("reg/codex@sha256:2", "codex", "broken-as-in-a-job")
        result = self.smoke("codex=reg/codex@sha256:2")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAILED codex: session/new: error", result.stdout)
        self.assertEqual(len(self.runs()), 1)

    def test_a_packaged_agent_kit_without_an_acp_adapter_fails(self):
        for name in ("claude", "codex", "pi", "pix"):
            self.image(f"reg/{name}@sha256:1", None)
        result = self.smoke(*(f"{name}=reg/{name}@sha256:1" for name in ("claude", "codex", "pi", "pix")))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        for name in ("claude", "codex", "pi", "pix"):
            self.assertIn(f"FAILED {name}: marsh's {name} Kit declares no ACP adapter", result.stdout)

    @unittest.skipIf(yaml is None, "needs PyYAML (make test-tools runs this under the publisher venv)")
    def test_loaded_images_take_the_acp_command_from_the_packaged_kit_sources(self):
        sources = json.loads((ROOT / "packaging/commands.json").read_text())
        for name, source in sources.items():
            self.images[f"marsh-validate/{pathlib.PurePath(source).name}:validate"] = {"behaviour": "good"}
        result = self.smoke("--sources", str(ROOT / "packaging/commands.json"), "--loaded", "marsh-validate")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.calls(["buildx", "imagetools"]), [])
        entrypoints = {run["image"]: run["entrypoint"] for run in self.runs()}
        for agent in ("claude", "codex", "pi"):
            self.assertEqual(entrypoints[f"marsh-validate/marsh-{agent}:validate"],
                             [f"/usr/local/bin/marsh-{agent}", "--acp"])
            self.assertRegex(result.stdout, rf"ok {agent}: .*session/prompt \(end_turn")
        self.assertIn("skipped shell", result.stdout)


if __name__ == "__main__":
    unittest.main()
