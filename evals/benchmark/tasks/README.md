# Benchmark tasks

The 16 pilot tasks: 9 sympy and 3 Django from SWE-bench Verified, and 4
self-made from fixes merged after 2026-09-29. Each holds `task.json`,
`gate.patch` (the visible check), `hidden.patch`, `gold.patch`, and for the
self-made ones `issue.md`. See [DESIGN.md](../DESIGN.md#tasks) and
[validation/](../validation/). The dry-run fixture lives in `../fixtures/`.
