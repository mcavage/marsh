#!/usr/bin/env python3
"""ACP smoke of built Kit images: the gate before a Kit's :latest moves.

For each image whose Kit declares an ACP adapter (com.docker.sandbox/acp@1),
start that adapter in the image the way marsh starts an ACP job (docker run
with marsh's user, capability and environment shape; a read-only /run/marsh
with job.json, context.md and a `marsh task-mcp` that serves MCP, so the
Kit's task-tool wiring runs as in a real job) and check, over stdio:

  initialize       a result, not an error;
  session/new      (cwd the job's folder, mcpServers []) a result with a
                   sessionId, not an error: Friday's codex image failed here;
  session/prompt   when this script knows how to point the agent at its fake
                   model endpoint (a local HTTP server that streams "ok"; no
                   credentials anywhere), a result with stopReason end_turn
                   within --prompt-timeout, after the agent called the
                   endpoint. ACP clients end a turn on that result.

The Kit's credential modes are set as if the host had bound a key
(SBX_CRED_<SERVICE>_MODE=apikey): the Kits then use proxy sentinels, which
reach only the fake endpoint here.

Images come from a command registry (--commands, as publish-kits.py writes
it: NAME -> REPOSITORY@DIGEST), from the local images `publish-kits.py
--validate-only --load NAME` built (--loaded NAME, with --sources), or
NAME=IMAGE arguments. The ACP command is the
image's Kit descriptor annotation; for a local image that has none, the
descriptor YAML of the Kit source named in --sources (packaging/commands.json).

Prints one line per Kit and exits 1 if any Kit failed. CI runs it on the
images it builds (ci.yml, `make kit-acp-smoke`) and on the pushed images
before any version or :latest tag moves (release.yml).
"""
from __future__ import annotations

import argparse
import http.server
import json
import os
import pathlib
import queue
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
DESCRIPTOR = "vnd.docker.sandbox.kit.descriptor"
ACP = "com.docker.sandbox/acp@1"
CONTEXT = ROOT / "crates/marsh-contracts/src/context.md"
# Where the job's folder and selected home are bound in the container.
PROJECT = "/marsh-smoke/project"
HOME = "/marsh-smoke/home"
# Every service a packaged Kit binds a credential for.
SERVICES = ("ANTHROPIC", "OPENAI", "GOOGLE", "MISTRAL", "XAI")
PROVIDER_HOST = "host.docker.internal"
REPLY = "ok"
# Pi-based agents: their Pi agent directory under the selected home.
PI_AGENT_DIRECTORIES = {"pi": ".pi/agent", "pix": ".pix-agent/agent"}

# `marsh task-mcp` as the Kits see it in a job: `--help` succeeds and the
# server answers MCP with marsh's three task tools (it runs no task).
MARSH_STUB = """#!/bin/sh
if [ "$1" = task-mcp ] && [ "$2" = --help ]; then
  echo 'Usage: marsh task-mcp'
  exit 0
fi
if [ "$1" = task-mcp ] && [ $# -eq 1 ]; then
  exec node /run/marsh/smoke-task-mcp.mjs
fi
echo "marsh (ACP smoke): $*: not available" >&2
exit 125
"""
TASK_MCP = """import { createInterface } from "node:readline";
const tools = ["task", "task_output", "task_apply"].map(name => ({
  name, description: `marsh ${name} (ACP smoke)`, inputSchema: {type: "object", properties: {}},
}));
const send = message => process.stdout.write(JSON.stringify({jsonrpc: "2.0", ...message}) + "\\n");
createInterface({input: process.stdin}).on("line", line => {
  let request;
  try { request = JSON.parse(line); } catch { return; }
  if (request.id === undefined) return;
  if (request.method === "initialize") {
    send({id: request.id, result: {protocolVersion: request.params?.protocolVersion ?? "2025-06-18",
      capabilities: {tools: {}}, serverInfo: {name: "marsh", version: "smoke"}}});
  } else if (request.method === "tools/list") {
    send({id: request.id, result: {tools}});
  } else if (request.method === "ping") {
    send({id: request.id, result: {}});
  } else {
    send({id: request.id, error: {code: -32601, message: "not available in the ACP smoke"}});
  }
});
"""


class SmokeError(Exception):
    pass


# --- the fake model endpoint -------------------------------------------------

class Provider(http.server.ThreadingHTTPServer):
    """Answers every chat, messages and responses request with a streamed "ok"."""

    daemon_threads = True

    def __init__(self, address=("0.0.0.0", 0)):
        super().__init__(address, ProviderHandler)
        self.requests = []
        self.lock = threading.Lock()
        threading.Thread(target=self.serve_forever, daemon=True).start()

    @property
    def port(self):
        return self.server_address[1]

    def count(self):
        with self.lock:
            return len(self.requests)


class ProviderHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            return self.json({"object": "list", "data": [{"id": "marsh-smoke", "object": "model"}]})
        self.send_error(404)

    def do_POST(self):
        try:
            length = int(self.headers.get("content-length") or 0)
            body = json.loads(self.rfile.read(length) or b"{}")
        except (ValueError, json.JSONDecodeError):
            body = {}
        path = self.path.split("?", 1)[0].rstrip("/")
        with self.server.lock:
            self.server.requests.append(path)
        stream = body.get("stream", False) is not False
        if path.endswith("/messages/count_tokens"):
            return self.json({"input_tokens": 5})
        if path.endswith("/chat/completions"):
            return self.chat(stream)
        if path.endswith("/messages"):
            return self.messages(stream)
        if path.endswith("/responses"):
            return self.responses_api()
        self.send_error(404)

    def json(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def events(self, events):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        for name, data in events:
            if name:
                self.wfile.write(f"event: {name}\n".encode())
            payload = data if isinstance(data, str) else json.dumps(data)
            self.wfile.write(f"data: {payload}\n\n".encode())
        self.wfile.flush()
        self.close_connection = True

    def chat(self, stream):
        usage = {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        base = {"id": "chatcmpl-smoke", "created": int(time.time()), "model": "marsh-smoke"}
        if not stream:
            return self.json({**base, "object": "chat.completion", "usage": usage, "choices": [
                {"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": REPLY}}]})
        chunk = {**base, "object": "chat.completion.chunk"}
        self.events([
            (None, {**chunk, "choices": [{"index": 0, "delta": {"role": "assistant", "content": REPLY},
                                          "finish_reason": None}]}),
            (None, {**chunk, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage}),
            (None, "[DONE]"),
        ])

    def messages(self, stream):
        message = {"id": "msg_smoke", "type": "message", "role": "assistant", "model": "marsh-smoke",
                   "stop_reason": None, "stop_sequence": None,
                   "usage": {"input_tokens": 5, "output_tokens": 1}}
        if not stream:
            return self.json({**message, "content": [{"type": "text", "text": REPLY}],
                              "stop_reason": "end_turn"})
        self.events([
            ("message_start", {"type": "message_start", "message": {**message, "content": []}}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                                     "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                     "delta": {"type": "text_delta", "text": REPLY}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                               "usage": {"output_tokens": 1}}),
            ("message_stop", {"type": "message_stop"}),
        ])

    def responses_api(self):
        item = {"type": "message", "id": "msg_smoke", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": REPLY, "annotations": []}]}
        response = {"id": "resp_smoke", "object": "response", "created_at": int(time.time()),
                    "model": "marsh-smoke", "status": "in_progress", "output": []}
        usage = {"input_tokens": 5, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 1,
                 "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 6}
        self.events([
            ("response.created", {"type": "response.created", "sequence_number": 0, "response": response}),
            ("response.output_item.added", {"type": "response.output_item.added", "sequence_number": 1,
                                            "output_index": 0, "item": {**item, "status": "in_progress",
                                                                        "content": []}}),
            ("response.output_text.delta", {"type": "response.output_text.delta", "sequence_number": 2,
                                            "item_id": "msg_smoke", "output_index": 0, "content_index": 0,
                                            "delta": REPLY}),
            ("response.output_item.done", {"type": "response.output_item.done", "sequence_number": 3,
                                           "output_index": 0, "item": item}),
            ("response.completed", {"type": "response.completed", "sequence_number": 4,
                                    "response": {**response, "status": "completed", "output": [item],
                                                 "usage": usage}}),
        ])


def prompt_setup(agent, home, url):
    """How agent AGENT reaches the fake endpoint: (environment, note), or
    None when this script does not know how (the prompt step is skipped)."""
    if agent in PI_AGENT_DIRECTORIES:
        # Pi's documented custom-provider files, in the Kit's agent directory
        # in the selected home.
        directory = home / PI_AGENT_DIRECTORIES[agent]
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "models.json").write_text(json.dumps({"providers": {"marsh-smoke": {
            "baseUrl": f"{url}/v1", "api": "openai-completions", "apiKey": "marsh-smoke",
            "models": [{"id": "marsh-smoke"}]}}}))
        (directory / "settings.json").write_text(json.dumps(
            {"defaultProvider": "marsh-smoke", "defaultModel": "marsh-smoke"}))
        return {}
    if agent == "claude":
        return {"ANTHROPIC_BASE_URL": url, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"}
    if agent == "codex":
        # codex-acp merges CODEX_CONFIG (config keys) into each session's
        # config; the Kit's entrypoint keeps a caller's keys.
        return {"CODEX_CONFIG": json.dumps({"openai_base_url": f"{url}/v1"})}
    return None


# --- images and descriptors --------------------------------------------------

class Docker:
    def __init__(self, program):
        self.program = program

    def output(self, *arguments, timeout=300):
        result = subprocess.run([self.program, *arguments], capture_output=True, text=True,
                                timeout=timeout, check=False)
        if result.returncode != 0:
            raise SmokeError(result.stderr.strip() or f"docker {arguments[0]} exited {result.returncode}")
        return result.stdout

    def annotated_descriptor(self, reference):
        """The Kit descriptor annotation of REFERENCE's linux/arm64 manifest, or
        None when the registry has none (a local image)."""
        try:
            top = json.loads(self.output("buildx", "imagetools", "inspect", "--raw", reference))
        except (SmokeError, json.JSONDecodeError, subprocess.TimeoutExpired):
            return None
        manifest = top
        for entry in top.get("manifests", []):
            platform = entry.get("platform", {})
            if platform.get("os") == "linux" and platform.get("architecture") == "arm64":
                repository = reference.rsplit("@", 1)[0] if "@" in reference else reference.rsplit(":", 1)[0]
                manifest = json.loads(self.output("buildx", "imagetools", "inspect", "--raw",
                                                  f"{repository}@{entry['digest']}"))
                break
        value = (manifest.get("annotations") or {}).get(DESCRIPTOR)
        return json.loads(value) if value else None

    def image_path(self, reference):
        if subprocess.run([self.program, "image", "inspect", reference], capture_output=True,
                          check=False, timeout=60).returncode != 0:
            self.output("pull", "--platform", "linux/arm64", "--quiet", reference, timeout=1800)
        env = json.loads(self.output("image", "inspect", "--format", "{{json .Config.Env}}", reference)) or []
        for item in env:
            if item.startswith("PATH="):
                return item[5:]
        return "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"


def source_descriptor(sources, name):
    """The descriptor YAML of Kit source NAME (packaging/commands.json: a
    directory relative to this checkout)."""
    try:
        import yaml
    except ImportError as error:
        raise SmokeError("reading a Kit source descriptor needs PyYAML "
                         "(scripts/requirements-publish.txt)") from error
    registry = json.loads(pathlib.Path(sources).read_text())
    if name not in registry:
        raise SmokeError(f"{name} is not in {sources}")
    root = ROOT / registry[name]
    for path in sorted(root.glob("*.yaml")) + sorted(root.glob("*.yml")):
        document = yaml.safe_load(path.read_text())
        if isinstance(document, dict) and document.get("schemaVersion") == "3" and "dockerfile" in document:
            return document
    raise SmokeError(f"no Kit descriptor in {root}")


def acp_adapter(descriptor):
    """(agent, command) of a descriptor's acp@1 capability, or None."""
    for capability in (descriptor or {}).get("capabilities") or []:
        if isinstance(capability, dict) and capability.get("type") == ACP:
            config = capability.get("config") or {}
            command = config.get("command")
            if (not isinstance(command, list) or not command
                    or not all(isinstance(word, str) and word for word in command)):
                raise SmokeError("acp@1 declares no command")
            return str(config.get("agent") or ""), command
    return None


# --- the ACP exchange ----------------------------------------------------------

class Agent:
    """One adapter process on stdio: JSON-RPC requests and their results."""

    def __init__(self, argv):
        self.process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE)
        self.messages = queue.Queue()
        self.stderr = []
        self.updates = []
        self.client_requests = []
        self.next_id = 0
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self._drain, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                message = {"invalid": line[:300].decode(errors="replace")}
            self.messages.put(message)
        self.messages.put(None)

    def _drain(self):
        for line in self.process.stderr:
            self.stderr.append(line.decode(errors="replace").rstrip())
            del self.stderr[:-40]

    def send(self, message):
        try:
            self.process.stdin.write((json.dumps({"jsonrpc": "2.0", **message}) + "\n").encode())
            self.process.stdin.flush()
        except (BrokenPipeError, OSError) as error:
            raise SmokeError(f"the adapter's stdin closed: {error}") from error

    def request(self, method, params, timeout):
        self.next_id += 1
        ident = self.next_id
        self.send({"id": ident, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeError(f"{method}: no result within {timeout:g}s")
            try:
                message = self.messages.get(timeout=remaining)
            except queue.Empty:
                continue
            if message is None:
                code = self.process.wait()
                raise SmokeError(f"{method}: the adapter exited ({code}) before its result")
            if "invalid" in message:
                raise SmokeError(f"{method}: the adapter wrote a line that is not JSON: {message['invalid']}")
            if "method" in message:
                self._client_side(message)
                continue
            if message.get("id") != ident:
                continue
            if "error" in message:
                raise SmokeError(f"{method}: error {json.dumps(message['error'])[:600]}")
            if not isinstance(message.get("result"), dict):
                raise SmokeError(f"{method}: no result object: {json.dumps(message)[:300]}")
            return message["result"]

    def _client_side(self, message):
        if message["method"] == "session/update":
            self.updates.append((message.get("params") or {}).get("update") or {})
        elif "id" in message:
            # The smoke client offers no fs, terminal or permission answers.
            self.client_requests.append(message["method"])
            self.send({"id": message["id"], "error": {"code": -32601, "message": "not supported by the ACP smoke"}})

    def close(self, kill):
        try:
            self.process.stdin.close()
        except OSError:
            pass
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            kill()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()


def agent_text(updates):
    text = ""
    for update in updates:
        if update.get("sessionUpdate") == "agent_message_chunk":
            content = update.get("content") or {}
            if content.get("type") == "text":
                text += str(content.get("text", ""))
    return text


def exchange(agent, timeouts, prompt, provider):
    """The checks, in order; returns what passed."""
    initialized = agent.request("initialize", {
        "protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False},
        "clientInfo": {"name": "marsh-acp-smoke", "version": "1"},
    }, timeouts["start"])
    if not isinstance(initialized.get("protocolVersion"), int):
        raise SmokeError(f"initialize: no protocolVersion in {json.dumps(initialized)[:300]}")
    session = agent.request("session/new", {"cwd": PROJECT, "mcpServers": []}, timeouts["session"])
    if not isinstance(session.get("sessionId"), str) or not session["sessionId"]:
        raise SmokeError(f"session/new: no sessionId in {json.dumps(session)[:300]}")
    passed = ["initialize", "session/new"]
    if not prompt:
        return passed
    before = provider.count()
    agent.updates.clear()
    result = agent.request("session/prompt", {
        "sessionId": session["sessionId"],
        "prompt": [{"type": "text", "text": f"Reply with exactly: {REPLY}"}],
    }, timeouts["prompt"])
    if result.get("stopReason") != "end_turn":
        raise SmokeError(f"session/prompt: stopReason {result.get('stopReason')!r}, not 'end_turn'")
    if provider.count() == before:
        raise SmokeError("session/prompt: ended without calling the model endpoint")
    if REPLY not in agent_text(agent.updates):
        raise SmokeError(f"session/prompt: the agent's message was {agent_text(agent.updates)[:200]!r}, "
                         f"not the endpoint's {REPLY!r}")
    passed.append("session/prompt (end_turn)")
    return passed


# --- one Kit image -------------------------------------------------------------

def job_capability(directory, name, context):
    """The read-only /run/marsh of a job: the in-job marsh, context and job.json."""
    run = directory / "run"
    (run / "bin").mkdir(parents=True)
    (run / "bin/marsh").write_text(MARSH_STUB)
    (run / "smoke-task-mcp.mjs").write_text(TASK_MCP)
    text = context.read_text() if context.is_file() else "# marsh: you are running as a marsh job\n"
    (run / "context.md").write_text(text)
    job = {"version": 1, "job": f"smoke-{name}", "name": name, "spawn": [name], "registered": [name],
           "socket": "/run/marsh/cap.sock", "path": "/run/marsh/bin", "limits": {}, "env": {}}
    (run / "job.json").write_text(json.dumps(job))
    for path in (run, run / "bin"):
        path.chmod(0o755)
    (run / "bin/marsh").chmod(0o755)
    for path in (run / "smoke-task-mcp.mjs", run / "context.md", run / "job.json"):
        path.chmod(0o644)
    return run


def smoke(docker, name, reference, adapter, args, provider):
    agent_name, command = adapter
    image_path = docker.image_path(reference)
    with tempfile.TemporaryDirectory(prefix=f"marsh-acp-smoke-{name}-") as temporary:
        directory = pathlib.Path(temporary).resolve()
        run = job_capability(directory, name, pathlib.Path(args.context))
        home = directory / "home"
        project = directory / "project"
        home.mkdir()
        project.mkdir()
        (project / "README.md").write_text("ACP smoke project\n")
        url = f"http://{PROVIDER_HOST}:{provider.port}"
        extra = prompt_setup(agent_name, home, url) if args.prompt else None
        for path in (home, project):
            path.chmod(0o777)
        container = f"marsh-acp-smoke-{name}-{secrets.token_hex(4)}"
        environment = {
            "HOME": HOME, "USER": "smoke", "LOGNAME": "smoke", "MARSH_SELECTED_HOME": HOME,
            "PATH": f"/run/marsh/bin:{image_path}", "MARSH_JOB": f"smoke-{name}", "MARSH_ENTRY": "1",
            **{f"SBX_CRED_{service}_MODE": "apikey" for service in SERVICES},
            **(extra or {}),
        }
        argv = [docker.program, "run", "--rm", "--interactive", "--init", "--name", container,
                "--platform", "linux/arm64", "--workdir", PROJECT, "--user", f"{os.getuid()}:{os.getgid()}",
                "--cap-drop", "ALL", "--security-opt", "no-new-privileges=true", "--pids-limit", "4096",
                "--add-host", f"{PROVIDER_HOST}:host-gateway",
                "--mount", f"type=bind,source={run},target=/run/marsh,readonly",
                "--mount", f"type=bind,source={home},target={HOME}",
                "--mount", f"type=bind,source={project},target={PROJECT}"]
        for key, value in sorted(environment.items()):
            argv += ["--env", f"{key}={value}"]
        argv += ["--entrypoint", command[0], reference, *command[1:]]
        timeouts = {"start": args.start_timeout, "session": args.session_timeout, "prompt": args.prompt_timeout}
        before_calls = provider.count()
        agent = Agent(argv)
        try:
            passed = exchange(agent, timeouts, extra is not None, provider)
        except SmokeError as error:
            tail = "\n".join(f"    {line}" for line in agent.stderr[-15:])
            requests = f" (agent->client requests: {', '.join(agent.client_requests)})" if agent.client_requests else ""
            with provider.lock:
                calls = provider.requests[before_calls:]
            requests += f" (model endpoint calls: {', '.join(calls) or 'none'})" if extra is not None else ""
            raise SmokeError(f"{error}{requests}" + (f"\n  adapter stderr:\n{tail}" if tail else "")) from error
        finally:
            agent.close(lambda: subprocess.run([docker.program, "kill", container], capture_output=True,
                                               check=False, timeout=60))
        if extra is None and args.prompt:
            passed.append(f"no prompt (no fake-endpoint setup for agent {agent_name!r})")
        return passed


def images(args):
    if args.commands:
        registry = json.loads(pathlib.Path(args.commands).read_text())
        if not isinstance(registry, dict):
            raise SmokeError(f"{args.commands}: not a command registry")
        selected = sorted(registry.items())
    else:
        selected = []
    if args.loaded:
        if not args.sources:
            raise SmokeError("--loaded needs --sources")
        registry = json.loads(pathlib.Path(args.sources).read_text())
        selected += [(name, f"{args.loaded}/{pathlib.PurePath(source).name}:validate")
                     for name, source in sorted(registry.items())]
    for item in args.image:
        name, separator, reference = item.partition("=")
        if not separator or not re.fullmatch(r"[a-z0-9][a-z0-9._-]*", name) or not reference:
            raise SmokeError(f"{item}: expected NAME=IMAGE")
        selected.append((name, reference))
    if not selected:
        raise SmokeError("no images: give --commands FILE, --loaded NAME or NAME=IMAGE")
    return selected


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("image", nargs="*", help="NAME=IMAGE (an image reference)")
    parser.add_argument("--commands", help="command registry JSON: NAME -> image reference")
    parser.add_argument("--sources", help="NAME -> Kit source directory (packaging/commands.json), "
                                          "for images without a descriptor annotation")
    parser.add_argument("--loaded", metavar="NAME",
                        help="every Kit in --sources as built by publish-kits.py --validate-only --load NAME")
    parser.add_argument("--context", default=str(CONTEXT), help="the job context.md to provide")
    parser.add_argument("--no-prompt", dest="prompt", action="store_false",
                        help="check initialize and session/new only")
    parser.add_argument("--start-timeout", type=float, default=180)
    parser.add_argument("--session-timeout", type=float, default=120)
    parser.add_argument("--prompt-timeout", type=float, default=120)
    parser.add_argument("--docker", default=os.environ.get("MARSH_SMOKE_DOCKER", "docker"))
    args = parser.parse_args(argv)
    if shutil.which(args.docker) is None:
        parser.exit(2, f"kit-acp-smoke: {args.docker}: not found\n")
    try:
        selected = images(args)
    except (SmokeError, OSError, json.JSONDecodeError) as error:
        parser.exit(2, f"kit-acp-smoke: {error}\n")
    docker = Docker(args.docker)
    provider = Provider()
    failed = []
    try:
        for name, reference in selected:
            try:
                descriptor = docker.annotated_descriptor(reference)
                if descriptor is None:
                    if not args.sources:
                        raise SmokeError("the image has no Kit descriptor annotation; give --sources")
                    descriptor = source_descriptor(args.sources, name)
                adapter = acp_adapter(descriptor)
                if adapter is None:
                    print(f"skipped {name}: its Kit declares no ACP adapter ({reference})", flush=True)
                    continue
                checks = smoke(docker, name, reference, adapter, args, provider)
                print(f"ok {name}: {', '.join(checks)} ({reference})", flush=True)
            except (SmokeError, OSError, json.JSONDecodeError, subprocess.SubprocessError) as error:
                failed.append(name)
                print(f"FAILED {name}: {error} ({reference})", flush=True)
    finally:
        provider.shutdown()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
