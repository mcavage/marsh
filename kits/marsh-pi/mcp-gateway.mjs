import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { Type } from "typebox";
import { fetch, ProxyAgent } from "undici";

const gateway = process.env.MCP_GATEWAY_URL;
const sentinel = process.env.MCP_SENTINEL_TOKEN_NAME;
const proxy = process.env.HTTP_PROXY;

export default function (pi) {
  if (!gateway && !sentinel) return;
  if (gateway !== "http://mcp-gateway.docker.internal/mcp" ||
      !/^[A-Za-z0-9._-]{1,128}$/.test(sentinel || "") ||
      !["http://gateway.docker.internal:3128", "http://192.0.2.1:8889"].includes(proxy) ||
      process.env.HTTPS_PROXY !== proxy) {
    throw new Error("invalid stock MCP Gateway environment");
  }
  const dispatcher = new ProxyAgent(proxy);

  async function connected(signal, action) {
    const deadline = AbortSignal.any([AbortSignal.timeout(120_000), ...(signal ? [signal] : [])]);
    const client = new Client({ name: "marsh-pi", version: "1" }, { capabilities: {} });
    const transport = new StreamableHTTPClientTransport(new URL(gateway), {
      requestInit: { headers: { Authorization: `Bearer ${sentinel}` } },
      fetch: (url, init) => fetch(url, { ...init, dispatcher }),
    });
    try {
      await client.connect(transport, { signal: deadline });
      return await action(client, deadline);
    } finally {
      await client.close().catch(() => {});
    }
  }

  async function tools(client, signal) {
    const found = [];
    let bytes = 0;
    let cursor;
    do {
      const page = await client.listTools(cursor ? { cursor } : undefined, { signal });
      bytes += Buffer.byteLength(JSON.stringify(page.tools));
      if (bytes > 1_000_000) throw new Error("MCP Gateway tool catalog is too large");
      found.push(...page.tools);
      if (found.length > 256) throw new Error("MCP Gateway has too many tools");
      cursor = page.nextCursor;
    } while (cursor);
    return found;
  }

  function result(value) {
    const text = JSON.stringify(value);
    if (Buffer.byteLength(text) > 1_000_000) throw new Error("MCP Gateway result is too large");
    return { content: [{ type: "text", text }] };
  }

  pi.registerTool({
    name: "mcp_gateway_list",
    label: "List MCP Gateway tools",
    description: "List MCP tools loaded into this Pi sandbox, including each tool's input schema.",
    parameters: Type.Object({}),
    async execute(_id, _params, signal) {
      return connected(signal, async (client, deadline) => result((await tools(client, deadline))
        .map(({ name, description, inputSchema }) => ({ name, description, inputSchema }))));
    },
  });

  pi.registerTool({
    name: "mcp_gateway_call",
    label: "Call MCP Gateway tool",
    description: "Call a tool loaded into this Pi sandbox. Use mcp_gateway_list first for its name and arguments.",
    parameters: Type.Object({
      name: Type.String(),
      arguments: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
    }),
    async execute(_id, params, signal) {
      const args = params.arguments || {};
      if (Buffer.byteLength(JSON.stringify(args)) > 256_000) {
        throw new Error("MCP Gateway arguments are too large");
      }
      return connected(signal, async (client, deadline) => {
        if (!(await tools(client, deadline)).some(tool => tool.name === params.name)) {
          throw new Error("MCP tool is not loaded in this sandbox");
        }
        return result(await client.callTool({ name: params.name, arguments: args },
          undefined, { signal: deadline }));
      });
    },
  });
}
