#!/usr/bin/env python3
"""Disposable read-only stdio MCP echo server; no credentials or dependencies."""
import json
import sys

tool = sys.argv[1]
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": request["params"]["protocolVersion"],
                  "capabilities": {"tools": {}}, "serverInfo": {"name": "marsh-env-echo", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": tool, "description": "Echo disposable fixture input",
                              "inputSchema": {"type": "object", "properties": {"input": {"type": "string"}}}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": request["params"]["arguments"]["input"]}]}
    elif method == "ping":
        result = {}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"],
                          "error": {"code": -32601, "message": "Method not found"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
