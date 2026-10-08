# Security policy

## Reporting a vulnerability

Please do not report security problems in public issues or pull requests.

Report via GitHub Security Advisories:
https://github.com/mcavage/marsh/security/advisories/new

Send a short description first; we will agree there on how to share anything
sensitive. Do not send credentials, tokens, or the contents of a guest home.

This is a small project maintained by Mark Cavage. There is no bounty and no
guaranteed response time, but reports are read and answered.

## What to include

- marsh version (`marsh --version`), `sbx version`, macOS version.
- Where the problem is: Mac host, shell VM, or Kit container, and which
  boundary you expected to hold (see the [security model](docs/security.md)).
- The starting access you assumed, what you expected, what happened, and the
  smallest reproduction with made-up data.
- Whether jobs were still running and whether cleanup was reported uncertain.

Prefer receipt fields (`marsh results show JOB --json`) and hashes to full
transcripts. Command arguments and paths can contain private information;
redact them. Keep original evidence in a private directory outside the project
and guest home, and do not reset or stop the scope just to tidy up a report.

## Scope

In scope: escaping a Kit container or VM to the Mac, reaching another Kit's
credentials or VM, a job gaining Docker, containerd, or `sbx` control, a split
argv branch writing outside its fork, marsh touching a VM it did not create,
and anything that undoes the [security model](docs/security.md).

Known and accepted, not vulnerabilities by themselves: `sbx` is trusted; the
path-based `sbx mount` reopen; shared trust between projects in one
`MARSH_HOME`; the authority you grant by publishing MCP or ACP tools or by
using `marsh --dev`; agents' own sandboxes being turned off inside containers.
Docker Sandboxes itself is Docker's product; report its bugs to Docker.

Test only systems and data you are allowed to use. Agent calls can cost money.
