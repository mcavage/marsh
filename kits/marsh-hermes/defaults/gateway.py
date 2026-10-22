"""Point Hermes's `mcp-gateway` server at the sandbox's MCP gateway.

Run by the Kit's entrypoint when SBX provides the gateway. Edits only that one
key, preserving the rest of config.yaml, and leaves a customized entry alone.
"""
import os
import sys

sys.path.insert(0, "/opt/hermes/src")
from ruamel.yaml import YAML  # noqa: E402
from utils import atomic_roundtrip_yaml_update  # noqa: E402

path = sys.argv[1]
want = {
    "url": os.environ["MCP_GATEWAY_URL"],
    "headers": {"Authorization": "Bearer " + os.environ["MCP_SENTINEL_TOKEN_NAME"]},
}
with open(path) as handle:
    config = YAML(typ="safe").load(handle) or {}
current = (config.get("mcp_servers") or {}).get("mcp-gateway")
ours = current is None or (
    isinstance(current, dict)
    and current.get("url") == want["url"]
    and set(current) <= {"url", "headers"}
)
if ours and current != want:
    atomic_roundtrip_yaml_update(path, "mcp_servers.mcp-gateway", want)
