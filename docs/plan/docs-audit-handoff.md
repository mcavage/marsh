# Homepage and docs audit handoff

This is the October 13 initial review. The later decisions and authorization
are in [the editorial brief](docs-site-refresh.md); the findings below describe
the documentation as it stood before the rewrite.

## Status

Initial audit completed October 13, 2026. No product copy, code, or agent
instructions were changed. This handoff is the only intentional repository
change from the audit. The existing untracked `docs/plan/profiles.md` belongs
to other work; leave it alone.

The user wants the marketing site, public docs, README, and AGENTS.md audited
because repeated agent rewrites have made them worse. They explicitly rejected
the initial suggestion that the guides were strong: the use cases are not
compelling and the guides themselves read like AI slop. Do not preserve them
merely because they include runnable commands and failure sections.

## Review instructions used

- `/Users/mcavage/dev/pix/skills/anti-slop/SKILL.md`
- `/Users/mcavage/dev/pix/skills/writing-voice/SKILL.md`
- `/Users/mcavage/dev/pix/agents/dx-consultant.md`

Both `~/dev/pix` and `~/dev/gm-pix-pack` were searched. The named anti-slop
and DX definitions were found in Pix. Apply concrete claims, direct writing,
progressive disclosure, one first-run path, and distinct tutorial/how-to/
reference/explanation purposes. Do not treat a banned-word scan as an editorial
review: the existing prose largely passes that scan and still reads generated.

## Main findings

- **Positioning is unresolved.** “Code mode for agents,” “Compose agents in
  Bash,” and “Agents are commands” do not explain why marsh improves on agent
  CLIs and ordinary shell scripts. Identify the user and their problem before
  picking another headline. The actual differences concern execution location,
  file access, separate parallel edits, and control of nested jobs.
- **Guides show mechanics, not value.** The race demonstrates two agents making
  the same one-line fix in a three-test Python project. The judge demonstrates
  paying a third agent to choose arbitrarily between identical patches. The
  panel finds an obvious `eval` issue. The farm-out guide repeats the toy fix
  and adds a large orchestration script and instructions snippet. Replace the
  examples only after choosing genuinely useful workflows; do not invent proof.
- **Writing problems are structural.** Repeated pitches, long model transcripts,
  generic advice, formulaic sections, and little authorial judgment. Another
  word-level polish will not fix this.
- **No dependable quickstart.** `docs/quickstart.md` assumes an imaginary Go
  project/test, two providers, and working Linux dependencies. Credentials come
  after the first agent example. Applying a result introduces another syntax
  and starts another split. Build one continuous path to a verified result.
- **Homepage overload.** The first demo combines split, nested fanout, collect,
  join, tee, a third agent, and a job tree. Six feature sections follow. Move
  advanced syntax and implementation details out of the first-contact path.
- **AGENTS lacks editorial boundaries.** It needs current scope and explicit
  approval requirements for positioning changes and broad rewrites. This is a
  recommendation, not permission to rewrite it now.

## Concrete defects to fix first

- `docs/split.md:9–26` uses successful bare `join`, then applies a patch from
  the removed results directory. Similar misleading patterns occur in
  `docs/man/marsh.1.md:268–275` and `docs/site/community.html:99–107`.
  Use `--keep` for later inspection/application. Verified against
  `crates/marsh/src/split_cli.rs:337–375` and
  `crates/marsh-daemon/src/split.rs:1540–1568` (source inspection, not a VM run).
- README first-run and quickstart cleanup imply `marsh stop` works while the
  marsh shell is still open. It refuses active shells/jobs. Show `exit` and
  distinguish Mac-terminal commands from guest-shell commands.
- Install, FAQ opening, and man-page NAME call marsh “Bash”; the default is
  Brush. Clarify optional `--shell bash` without contradicting the no-Bash-
  implementation rule.
- `AGENTS.md:23–24` excludes private split workspaces, the advertised feature.
- Shell/FAQ say 49 Brush patches, design compatibility says 98, and there were
  99 patch files at audit time. Avoid manually repeating this count.
- Architecture describes gix-based splits; workspace implementation notes say
  “No gix.” Current behavior must not require reconciling historical sections.
- Linked worktrees are refused for splits and ignored input files are omitted.
  These restrictions belong in public onboarding/split docs, not only design
  notes. See `split_fs.rs:410–422` and `docs/design/workspaces.md:461–477`.
- Security’s “never holds your real keys” conflicts with documented in-container
  login files in the shared guest home. Scope proxy-credential claims and host-
  execution claims to their actual boundaries and explicit grants. This was a
  documentation review, not a security audit.
- `CONTRIBUTING.md:8–9` overstates guide test coverage. The harness runs marked
  examples, often replacement scripts, not every transcript or the full first-
  run journey. See `tests/acceptance/workspaces_uat.py:948–968`.
- Panel guide switches from uncommitted changes to `main...HEAD` mid-guide.

## Recommended sequence

1. Agree with the user on audience, problem, and product story.
2. Choose convincing examples and remove redundant/toy workflows.
3. Repair factual contradictions and broken examples.
4. Build one first-run tutorial, then rewrite homepage and README around it.
5. Separate task guides, reference, explanation, and historical design records.
6. Add editorial boundaries and complete-journey checks to prevent drift.

No broad rewrite was authorized or started. The immediate request was an audit,
followed by this quick handoff before reboot.

## Evidence and checks

Read all public manual pages, four guides, README, AGENTS, and man-page source;
reviewed published design organization/consistency and the live homepage.
Rendered local desktop and emulated mobile views. Site build and generated-site
and repository Markdown link checks passed with zero broken links. No actual
install-to-success timing, billed agent runs, or SBX acceptance runs were done.

Temporary build/screenshots are under
`/private/var/folders/zr/1nbpv_812d17xnm1kdwwvq1m0000gn/T/opencode/`, with names
starting `marsh-docs-audit-`; they may not survive reboot. They are not required
to resume. Rebuild with `python3 scripts/sitegen.py --out <temporary-dir>`.
