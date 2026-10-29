# Idea: container-scoped credentials and egress in Docker Sandboxes

Status: proposal for the Docker Sandboxes (sbx) and auth teams. Not built.
marsh is the reference user; nothing here changes marsh today.

## Problem

sbx enforces network policy and credential injection per sandbox VM, at the
VM's egress proxy. Every process in the VM shares one allowlist and one set
of injected credentials. That's the right unit when a VM holds one agent.
It breaks down when a VM holds an agent **and** the narrow tools it calls.

marsh runs each command as a fresh container inside a warm VM for its Kit.
For a tool such as `gog` (Google Workspace), the safety rules live in the
tool. Its wrapper allows only listed commands: read, draft, and create, but
never send, share, or delete. The OAuth grant can't enforce that, because
Google lets `gmail.compose` send mail. So the rules only hold if the **agent
can't reach the token directly**.

If `gog` ran in the agent's VM, the VM would need `gmail.googleapis.com` in
its allowlist and the Google token in its proxy. The agent, which reads
untrusted email and so is the main prompt-injection target, could then
`curl` Gmail's send endpoint, and the proxy would attach the token. The
wrapper would become optional.

marsh's answer today is a VM boundary. The tools Kit (`kits/marsh-tools`:
`gog`, `bamboohr`) has its own VM, network policy, and credentials. Agents
reach it only by starting a job through the marsh daemon. That works, but it
costs a VM per trust domain: memory, a cold start, and another thing that
can be quarantined.

## Proposal

Let the identity in an egress rule or credential binding be a **container in
the VM**, not only the VM. A credential capability or allowlist entry would
name a workload selector. The proxy would match a request to its source
container and apply that container's rules.

Sketch (Kit v3 descriptor):

```yaml
capabilities:
  - type: com.docker.sandbox/credential@1
    config:
      service: gog
      scope: {workload: gog}          # new: only containers labelled workload=gog
      apiKey:
        name: GOG_ACCESS_TOKEN
        inject: [{domain: gmail.googleapis.com, header: Authorization, format: "Bearer %s"}]
  - type: com.docker.sandbox/network-policy@1
    config:
      runtime:
        allow:
          - {host: gmail.googleapis.com:443, workload: gog}   # new: per-workload allow
```

The VM owner (marsh's worker, root in the VM) labels each job container
when it creates it. The proxy keys rules on that label and the container's
network identity. For example, sbx could use a per-workload bridge network,
or proxy credentials scoped to the container's network namespace. A
container with no label gets only the rules that have no workload scope,
which is today's behavior.

## Requirements

1. **Unforgeable attribution.** A container can't claim another container's
   identity. The label comes from the VM's root (the container runtime), not
   from the workload. Source-IP matching only works if containers can't change
   their address or join another container's network namespace. A
   per-container proxy credential that the runtime injects and the workload
   can't read would also work.
2. **Fail closed.** A request that can't be attributed to a container gets
   the unscoped rules only, never a scoped credential.
3. **No new trust in the guest workload.** The trust root stays sbx and the
   VM's root, as today. marsh's jobs already get no Docker, containerd, or sbx
   socket, so they can't relabel themselves.
4. **Inspectable.** `sbx inspect` lists each scoped rule with its workload,
   so a user can check that `gog`'s token is bound only to `gog`.
5. **Same shape on cloud sandboxes**, so marsh's cloud placement keeps one
   model.

## What marsh would do with it

- Run tool jobs in the agent's VM, with no separate tools VM, cold start, or
  quarantine domain. This needs `ProcessRun` to place a child in the
  parent's VM, which marsh's lineage tree, admission and cleanup checks
  already handle per container.
- Keep the VM boundary as an option for Kits that want a separate kernel.
- Model it as one more `Process.tla` right: a child's egress and credentials
  come from its own Kit, never from its parent. `NoHomeKitHome`
  (`org.marsh/home@1`) is the same rule for the guest home.

## Open questions

- Attribution mechanism: per-container networks, a per-container proxy
  credential (`HTTPS_PROXY` user:pass minted by the runtime), or eBPF on the
  VM's bridge. Which survives container restarts and is cheapest per
  container?
- Interaction with global versus per-sandbox `sbx secret` scope, and with
  the host-domain approvals in `~/.config/sbx/credentials.yaml`.
- Whether MCP gateway servers (also per VM today) need the same scoping.

## References

- marsh security model: `docs/security.md`; Kit VM and job container trust
  boundaries: `docs/architecture.md`.
- Tools Kit and why it has its own VM: `kits/marsh-tools/README.md`.
- Child rights never widen: `docs/model/Process.tla` (`Attenuation`,
  `SpawnSetAttenuates`, `NoHomeKitHome`).
