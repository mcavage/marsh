# Documentation verification, October 14, 2026

This records checks for the editorial rewrite, not a product benchmark or a
release acceptance run. The source changes are documentation, site generation,
installer help text, and marked-example discovery. Runtime Rust code, formal
models, benchmark tasks, and the separate CI work are unchanged.

## What was reviewed

The pass covers the root README and contributor/agent instructions, every
Markdown page under `docs/`, and the site's HTML, metadata, captions, and replay
labels. Historical design and patch records retain their evidence and formal
statements. They are distinguished from current operating instructions.

Gemini 4 Argon reviewed the prose adversarially in groups, followed by revisions
and a combined public-documentation pass. This is an editorial review, not an
independent security audit. The review criteria are in the
[editorial brief](docs-site-refresh.md).

## Executed checks

| Check | Result and scope |
|---|---|
| Marked public examples, W29 | 38 cases passed in fresh isolated repositories, plus two meaningful negative controls |
| Agent-job examples, P31 | Six cases passed; the result was reused only after checking the relevant input and artifact hashes |
| Three-way merge controls | Removing the split object-directory export made both host and guest variants fail because Git lacked the base blob; the complete example merged successfully |
| Real provider access | One Claude invocation returned `READY`, with tools and child launches disabled; container deletion was confirmed |
| ACP | Reservation, readiness, turns, permissions, cancellation, and stop passed with the deterministic ACP agent |
| Published ACP | Host-exporter and stock-Gateway turns, cancellation, permissions, retry idempotency, revocation, and fresh-generation access passed |
| Published MCP pipeline | Real stock-Gateway calls checked output, bounds, failure status, and cancellation |
| Revocation | Old exporters and Gateway loads were denied after replacement; a fresh generation worked |
| MCP stdio | Fourteen protocol checks passed through `marsh-mcp` and `marsh mcp serve`, using deterministic downstream programs |
| Installation | A verified release archive installed into a new disposable prefix; checksum rejection, help, version, config checking, and man pages passed |
| Site | 38 pages built; generated-site and repository Markdown links passed |
| Browser | Desktop light/dark, 320/390px mobile, and tablet layouts; replay, Stop, mobile controls, copying, keyboard navigation, reduced motion, and no-JavaScript reading passed |
| Generated primers | No internal `doc-test` annotations remain in `llms.txt` or `llms-full.txt` |

W29 now discovers marked examples throughout the public manual, guides, root
README, man source, and agent primer. P31 separately exercises `docs/agents.md`.
Declared fixture substitutions test the surrounding command and patch handling.
They do not establish the quality of an agent's implementation or review.

The tests found four fixtures that confused a JSON split handle with its ID.
They now obtain `SPLIT_ID` from `join`'s consumer environment. Two old controls
removed an index refresh that was unnecessary on the tested release. Their
replacement exercises a genuine uncommitted merge base. A malformed first
attempt at that replacement was also caught and retained in the test history.

The final editorial nits changed prose only. A comparison of all 44 extracted
examples confirmed identical scripts, fixture substitutions, expected statuses,
stream assertions, preparation, and negative controls; only source line numbers
were excluded from that comparison.

## Artifacts and limits

The installed test bundle was
`v0.1.4-nightly.20261014011818.gb5ca1b6`, with stock SBX 0.47.0. Its archive
SHA-256 was
`70eef273dcccc8ee1ffe4b4ab10559dd9ac53c857c744a79677259e0fb0a9a73`.
The fixture was
`docker.io/mcavage/marsh-fixture@sha256:76614940e7c61929acaf12bf5d86d1988ca09c9631eed3afe48fd1c5b1fecbc9`.
The release files were verified against the downloaded archive; there is no
build receipt binding that binary to this documentation checkout.

The provider smoke reported model `claude-opus-5-5`, one turn, and a CLI-estimated
cost of $0.023713. This checks configured provider access, not the Quickstart's
repository-specific answer or a provider-account billing statement.

Installation used a downloaded local archive. Global Homebrew operations,
interactive credential entry, the network `curl | sh` path, and user-install
removal were not executed. Project-specific CLI/API tasks are adaptable recipes
with stated prerequisites, not captured successful runs. Output excerpts and
historical commands are not counted as executable tests.

All test-created VM identities were recorded and checked before cleanup.
Foreign VM names and UUIDs were unchanged. No real user install, client config,
guest home, or project was used as the test workspace.

## Evidence

Machine-readable reports and failed attempts are retained under the approved
OpenCode temporary directory. They may not survive a reboot:

- `marsh-example-prep-pqj8keoi/final-validation-ledger.json`
- `marsh-acp-mcp-final.VatC7Q/integration-final.json`
- `marsh-published-acp-final.HuI4FR/summary.json`
- `marsh-editorial-browser/report.json`
- `marsh-doc-example-inventory.json`

The inventory distinguishes executable blocks from adaptable commands,
configuration/output examples, host-mutating instructions, and historical
references. It is a coverage map, not an assertion that every fenced block was
run literally against the user's machine.
