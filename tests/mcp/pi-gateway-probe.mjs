// Stock-Gateway fixture caller of the shipped Pi extension, without inference.
// Run inside a disposable Kit with fixture_probe loaded and compact JSON output.
import factory from "/opt/marsh/pi-gateway/mcp-gateway.mjs";
const tools = new Map();
const handlers = new Map();
factory({ registerTool: tool => tools.set(tool.name, tool),
  on: (name, handler) => handlers.set(name, handler) });
let count = 0;
const invoke = async (name, args) => {
  const value = await tools.get("mcp_gateway_call").execute(String(++count),
    {name, arguments: args}, undefined);
  return JSON.parse(value.content[0].text);
};
try {
  const listed = await tools.get("mcp_gateway_list").execute("list", {}, undefined);
  if (!JSON.parse(listed.content[0].text).some(t => t.name === "fixture_probe")) throw new Error("missing typed tool");
  const created = await invoke("code-mode", {tools:["fixture_probe"]});
  const name = JSON.stringify(created).match(/codemode-exec-[a-z0-9]+/)?.[0];
  if (!name) throw new Error("missing executor");
  const execution = await invoke(name, {code:"const r=JSON.parse(await tools.fixture_probe({value:'pi-extension'})); if (!r.ok) throw new Error(r.error); return r.result;"});
  const output = JSON.parse(execution.content[0].text);
  if (!output.injected || output.value !== "pi-extension") throw new Error("wrong extension result");
  console.log(JSON.stringify({pi_extension:true, dynamic_executor:true, authenticated_fixture_read:true}));
} finally {
  await handlers.get("session_shutdown")();
}
