import readline from "node:readline";

const sessionId = "synthetic-acp-session";
let pendingPrompt = null;
let pendingPermission = null;
let cancelBurst = false;
let nextPermissionId = 100;

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
      send({ id, result: { sessionId } });
      // Real agents announce their command list right after session/new.
      // It is session state, not a stray update: status must not report it.
      send({ method: "session/update", params: { sessionId, update: {
        sessionUpdate: "available_commands_update", availableCommands: [] } } });
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
        inputLines.pause();
        setTimeout(() => process.exit(0), 2000); // bounded hostile peer lifetime
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
