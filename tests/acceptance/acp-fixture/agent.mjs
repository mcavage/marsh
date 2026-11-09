import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import readline from "node:readline";

const sessionId = "synthetic-acp-session";
let pendingPrompt = null;
let pendingPermission = null;
let cancelBurst = false;
let nextPermissionId = 100;
let execChild = null;
const EXEC_LIMIT_MS = 300000;
// base64 grows output by a third: one `fixture:out:` update carries at most
// this many raw bytes (a multiple of 3, so the chunks' base64 concatenates),
// which stays under the 300000-byte update bound. Larger output is sent as
// several updates; output beyond EXEC_MAX_BYTES fails loudly, never truncates.
const EXEC_CHUNK_BYTES = 196608;
const EXEC_MAX_BYTES = 32 * 1024 * 1024;

// The self-test's break (HARNESS-03): NFX_FAKE_EXEC=off in the agent's
// environment, or a `.nfx-fake-exec-off` file in its cwd, turns the verb off.
function execDisabled() {
  return process.env.NFX_FAKE_EXEC === "off" || fs.existsSync(".nfx-fake-exec-off");
}

// `exec SH`: what an agent's shell tool does. Runs `sh -c SH` in this
// process's cwd for at most 300 s, then replies `fixture:exit:N` (the shell's
// status: 128+signal when it was killed, 124 on the time limit) and
// `fixture:out:BASE64` with ALL of stdout, split over several `fixture:out:`
// updates when it exceeds EXEC_CHUNK_BYTES (concatenate the decoded chunks;
// stderr goes to this process's stderr). Output over EXEC_MAX_BYTES kills the
// command and replies `fixture:exit:125` and `fixture:error:output-limit`.
function runExec(id, script) {
  const child = spawn("sh", ["-c", script], { cwd: process.cwd(), detached: true,
    stdio: ["ignore", "pipe", "inherit"] });
  execChild = child;
  const chunks = [];
  let total = 0;
  let timedOut = false;
  let overLimit = false;
  child.stdout.on("data", (chunk) => {
    total += chunk.length;
    if (total > EXEC_MAX_BYTES) {
      overLimit = true;
      try { process.kill(-child.pid, "SIGKILL"); } catch {}
      return;
    }
    chunks.push(chunk);
  });
  const timer = setTimeout(() => {
    timedOut = true;
    try { process.kill(-child.pid, "SIGKILL"); } catch {}
  }, EXEC_LIMIT_MS);
  const finish = (status) => {
    clearTimeout(timer);
    if (execChild !== child) return;
    execChild = null;
    if (pendingPrompt !== id) return;
    if (overLimit) {
      update("fixture:exit:125");
      update("fixture:error:output-limit");
      endPrompt(id, "refusal");
    } else {
      update(`fixture:exit:${timedOut ? 124 : status}`);
      const output = Buffer.concat(chunks);
      for (let at = 0; at === 0 || at < output.length; at += EXEC_CHUNK_BYTES) {
        update(`fixture:out:${output.subarray(at, at + EXEC_CHUNK_BYTES).toString("base64")}`);
      }
      endPrompt(id, "end_turn");
    }
    pendingPrompt = null;
  };
  child.on("error", () => finish(127));
  child.on("close", (code, signal) =>
    finish(code ?? (signal ? 128 + (os.constants.signals[signal] ?? 0) : 1)));
}

function send(value) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...value })}\n`);
}

function update(text) {
  send({
    method: "session/update",
    params: {
      sessionId,
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text },
      },
    },
  });
}

function endPrompt(id, stopReason) {
  send({ id, result: { stopReason } });
}

const inputLines = readline.createInterface({ input: process.stdin });
for await (const line of inputLines) {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    continue;
  }
  const { id, method, params } = request;
  switch (method) {
    case "initialize":
      send({ id, result: { protocolVersion: 1 } });
      break;
    case "session/new":
      // Real agents announce their command list around session/new. It is
      // session state, not a stray update: status must not report it. It is
      // written before the response so the caller has consumed it by the time
      // session/new returns; after the response it races the caller's first
      // prompt, and a prompt that wins the race would own the announcement as
      // turn output (a loaded machine did exactly that).
      send({ method: "session/update", params: { sessionId, update: {
        sessionUpdate: "available_commands_update", availableCommands: [] } } });
      send({ id, result: { sessionId } });
      break;
    case "session/prompt": {
      const text = params?.prompt?.[0]?.text;
      update(`fixture:${text}`);
      if (text === "unknown-update") {
        send({ method: "session/update", params: {
          sessionId, update: { sessionUpdate: "future_fixture_update", value: "visible-gap" },
        } });
      }
      if (text === "control-flood") {
        // Stop reading stdin until the caller creates the release file, or
        // exit without reading it if the caller creates the exit file. The
        // 60 s bound only keeps a failed caller from leaving a stuck peer.
        inputLines.pause();
        const release = process.env.MARSH_FIXTURE_RELEASE;
        const exit = process.env.MARSH_FIXTURE_EXIT;
        const started = Date.now();
        const timer = setInterval(() => {
          if (exit && fs.existsSync(exit)) process.exit(0);
          if ((release && fs.existsSync(release)) || Date.now() - started > 60000) {
            clearInterval(timer);
            inputLines.resume();
          }
        }, 10);
        pendingPrompt = id;
        for (let n = 0; n < 200; n++) send({id:10000+n, method:"future/"+"x".repeat(30000), params:{}});
      } else if (text === "lossy") {
        send({method: "session/update", params: {sessionId, update: {sessionUpdate: 9}}});
        endPrompt(id, "end_turn");
      } else if (text === "burst-400" || text === "burst-hold") {
        for (let n = 0; n < 400; n++) update(`b${n};`);
        if (text === "burst-hold") pendingPrompt = id;
        else endPrompt(id, "end_turn");
      } else if (text === "diff") {
        send({method:"session/update", params:{sessionId, update:{sessionUpdate:"tool_call",
          toolCallId:"edit-1", title:"Edit a.txt", kind:"future_edit", status:"future_completed",
          content:[{type:"diff",path:"a.txt",oldText:"old",newText:"new"},
                   {type:"content",content:{type:"future_block",data:{opaque:42}}}],
          locations:[{path:"a.txt",line:3}], _meta:{fixture:"metadata"}}}});
        send({method:"session/update", params:{sessionId, update:{sessionUpdate:"available_commands_update",
          availableCommands:[{name:"review",description:"Review"}], _meta:{future:true}}}});
        endPrompt(id,"end_turn");
      } else if (text === "slow-6") {
        pendingPrompt = id;
        let n = 0;
        const timer = setInterval(() => {
          if (pendingPrompt !== id) { clearInterval(timer); return; }
          update(`u${n++};`);
          if (n === 6) { clearInterval(timer); endPrompt(id,"end_turn"); pendingPrompt = null; }
        }, 200);
      } else if (text === "agent-error") {
        send({id,error:{code:-32001,message:"SECRET /host/private \\u001b[31m".repeat(10000)}});
      } else if (text === "oversize-update") {
        update("x".repeat(300000));
        endPrompt(id,"end_turn");
      } else if (text?.startsWith("write ")) {
        // `write PATH TEXT`: what a lane agent's edit can reach.
        const [, path, ...words] = text.split(" ");
        try {
          fs.writeFileSync(path, words.join(" "));
          update("fixture:wrote");
        } catch (error) {
          update(`fixture:refused:${error.code}`);
        }
        endPrompt(id, "end_turn");
      } else if (text?.startsWith("read ")) {
        try {
          update(`fixture:read:${fs.readFileSync(text.slice(5), "utf8")}`);
        } catch (error) {
          update(`fixture:refused:${error.code}`);
        }
        endPrompt(id, "end_turn");
      } else if (text?.startsWith("exec ") && !execDisabled()) {
        pendingPrompt = id;
        runExec(id, text.slice(5));
      } else if (text === "late") {
        endPrompt(id, "end_turn");
        update("late-after-end");
      } else if (text === "cancel-burst") {
        pendingPrompt = id;
        cancelBurst = true;
      } else if (text === "hold") {
        pendingPrompt = id;
      } else if (text === "malformed") {
        process.stdout.write('{"jsonrpc":"2.0","method":"session/update","params":\n');
        update("fixture:after-malformed");
        endPrompt(id, "end_turn");
      } else if (text === "idle-malformed") {
        endPrompt(id, "end_turn");
        setTimeout(() => {
          process.stdout.write('{"jsonrpc":"2.0","method":"session/update","params":\n');
        }, 200);
      } else if (text === "permission" || text === "always-only") {
        pendingPrompt = id;
        pendingPermission = nextPermissionId++;
        send({
          id: pendingPermission,
          method: "session/request_permission",
          params: {
            sessionId,
            toolCall: { toolCallId: "fixture-tool", title: "Fixture permission" },
            options: [
              ...(text === "always-only" ? [] : [
                { optionId: "once", name: "Allow once", kind: "allow_once" },
                { optionId: "deny", name: "Deny once", kind: "reject_once" },
              ]),
              { optionId: "always", name: "Allow always", kind: "allow_always" },
            ],
          },
        });
      } else {
        endPrompt(id, "end_turn");
      }
      break;
    }
    case "session/cancel":
      if (execChild) {
        try { process.kill(-execChild.pid, "SIGKILL"); } catch {}
        execChild = null;
      }
      if (pendingPrompt !== null) {
        if (cancelBurst) {
          for (let n = 0; n < 200; n++) update(`c${n};`);
          cancelBurst = false;
        }
        endPrompt(pendingPrompt, "cancelled");
        pendingPrompt = null;
      }
      break;
    default:
      if (id === pendingPermission && pendingPrompt !== null) {
        const allowed = request.result?.outcome?.outcome === "selected"
          && request.result?.outcome?.optionId === "once";
        update(`fixture:permission:${allowed ? "allowed" : "denied"}`);
        endPrompt(pendingPrompt, allowed ? "end_turn" : "refusal");
        pendingPrompt = null;
        pendingPermission = null;
      }
  }
}
