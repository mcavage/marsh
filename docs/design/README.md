# Design documents

These documents explain the implementation and its contracts for contributors
and reviewers. They describe current behavior; earlier proposals and review
rounds are in the Git history. To use marsh, read the [manual](../README.md).

Read in order:

1. [Local product contract](../plan/local-product-contract.md): the
   user-visible contract
2. [Architecture](../architecture.md): crates, data flow, trust boundaries
3. [TLA+ model](../model/README.md): cross-component state, invariants, and
   where each action lives in the code
4. [Acceptance contract](../../tests/acceptance/CONTRACT.md): black-box
   observations a release must pass

By topic:

| Document | Subject |
|---|---|
| [processes.md](processes.md) | Nested jobs: lineage, admission, the job capability socket ([acceptance](processes-acceptance.md)) |
| [workspaces.md](workspaces.md) | Daemon-owned splits and forks ([acceptance](workspaces-acceptance.md)) |
| [stock-sbx-adapter.md](stock-sbx-adapter.md) | The only boundary to `sbx`: ownership map, mounts, workers |
| [shell-attachments.md](shell-attachments.md) | Attaching a shell, interruption, cleanup evidence |
| [command-registry.md](command-registry.md) | Command-to-Kit mappings and their rules |
| [results.md](results.md) | Job records behind `marsh results`, and scope configuration |
| [bash-compatibility.md](bash-compatibility.md) | Known Bash differences and the differential tests |
| [mcp.md](mcp.md) | The host MCP server and published-command export ([test contract](../../tests/mcp/CONTRACT.md)) |
| [ACP acceptance](../../tests/acceptance/ACP.md) | ACP sessions and their publication |
| [kit-publication.md](kit-publication.md) | Building and publishing Kits |
| [supply-chain.md](supply-chain.md) | Build inputs, published images, notices |
| [self-development.md](self-development.md) | `marsh --dev` and the dev broker |
| [Brush patch record](../upstream/brush/README.md) | The upstream Brush revision and marsh's patch queue |

Cloud placement (session-origin slice shipped as a preview; the rest proposed):

- [Cloud placement](cloud-placement.md): shell-native remote commands, automatic
  delta return, nested placement, model obligations, and acceptance gates.
- [Detached cloud work](cloud-detach.md) (proposed): start cloud work, close the
  lid, and collect output and file changes later.
- [Session-origin dispatch](../plan/cloud-dispatch.md): what is implemented,
  where, and its live evidence.
- [Live cloud feasibility findings](../plan/cloud-feasibility.md): tested Kit v2
  execution, two-policy sharing, limitations, and verified resource cleanup.
- [Owned cloud lifecycle](../plan/cloud-lifecycle.md): implemented adapter,
  real-caller qualification, and cloud ownership model.
- [Bring your own agent Kit](../plan/custom-agent-kit.md).
