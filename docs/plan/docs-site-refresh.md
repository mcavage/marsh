# Site and documentation refresh

## Status

October 13, 2026. Homepage direction agreed with the user, followed by approval
to proceed. A local homepage candidate is implemented in `docs/site/`, including
its metadata, social card, styles, and static copyable examples. The user
approved publishing this first pass for live review because the site has not
been shared. PR #77 is merged, Pages deployment succeeded, and the new homepage
was verified at `https://runmar.sh`. A follow-up
branch contains factual documentation repairs. This document is not authorization
for an autonomous site-wide rewrite.

Read [the audit handoff](docs-audit-handoff.md) for existing defects and evidence.
Leave the unrelated `profiles.md` work alone.

## Agreed homepage direction

Present marsh as a platform for composing agents, with concrete execution
guarantees. Do not position it as a single-outcome product, a bug-fixing race
tool, or an experimentation playground.

The headline establishes what marsh is. The demonstration reveals what ordinary
shell scripts leave the developer managing.

The story has three parts:

- Agents are composable processes, including from inside other agents.
- Parallel work can use separate project copies for concurrent edits, with
  inspectable patches and explicit application.
- Execution environments, nested job lifetimes, cancellation, and cleanup have
  defined behavior.

Avoid an unqualified promise of safety. Direct agent calls can change or delete
project files. Private agent mounts and trusted shell branches have different
boundaries. Passing tests does not prove a patch is safe. Scope credential
claims to proxy-managed credentials rather than saved guest login files.

## Proposed homepage structure

1. Hero: identity, concrete difference, one primary getting-started action.
2. Compact substantive demonstration: ordinary commands and tests, parallel
   edits on separate copies, returned patches, no automatic merging.
3. Deeper capability: the same registered commands work inside an agent's shell
   tool, with nested calls tracked and cancelled as one process tree.
4. Why not scripts and worktrees: explain the execution and lifecycle work marsh
   manages, without claiming those tools cannot compose agents.
5. Execution boundaries and practical requirements, followed by one onboarding
   destination.

Keep GitHub available, but do not make starring compete with getting started.
Keep the Apple Silicon requirement beside the first action. Explain Linux
compatibility, credentials, cold starts, model-account costs, and early/unsigned
status without requiring a reader to discover consequential limits elsewhere.

Move the nested opening pipeline, automatic judge, long patch-selection script,
numeric admission limits, MCP/ACP walkthroughs, shell-selection examples, and
daemon ownership diagram out of first contact. Their removal from the homepage
does not imply removing the underlying documentation or product features.

## Working hero, not final copy

> **A shell for composing agents.**
>
> Run Claude, Codex, and Pi as commands, each in a fresh Linux container. Compose
> them with pipes and scripts, run concurrent edits on separate copies, and
> manage agents started by agents in one process tree.

## Review lenses

Use these as editorial questions, not attributed opinions or fabricated quotes:

- Patrick Collison: does each sentence earn the reader's attention, and does the
  evidence establish what marsh adds to existing agent CLIs and scripts?
- Tobi Lütke: is using the platform simpler than explaining its machinery?
- Solomon Hykes: do a few understandable primitives compose naturally, including
  into uses the page does not enumerate?
- Mitchell Hashimoto: can the reader predict execution location, file access,
  and lifecycle behavior?
- Simon Willison: can the demonstration be reproduced, and are observed results
  distinguished from guarantees and unverified claims?

Apply Pix's impatient-developer, DX, DevRel, design, and editorial guidance.
Do not mistake a banned-word scan for a substantive editorial review.

## Next decisions and sequence

First develop the hero and demonstration together. The actual task, repository,
prerequisites, and captured results remain undecided. The candidate uses an
explicitly illustrative events API/CLI recipe, not a recorded or verified agent
run. Do not invent a successful transcript, speed claim, or productivity claim. The demonstration
must be useful enough to justify its parallel work and concise enough not to
require learning every marsh feature first.

Review the full first-run path before shipping a homepage that sends readers to
it. Then proceed in the user's requested order: guides and manual, then the rest
of the public documentation and site. Those phases have no approved rewrite
plan yet. Resolve factual contradictions against current behavior rather than
repeating historical design claims.

## Candidate review and validation

Pix-guided independent editorial and visual reviews led to these revisions:
shared-file composition and custom commands are visible without additional
feature panels; stdout and exit status explain the delegation interface;
section headings share one scale; mobile examples precede explanation; short
code lines remain intact; safety qualifications use plain language.

The animated transcript and its replay code were removed. The social card now
matches the headline. Existing shared documentation styles and community
content were otherwise left in place.

Checks performed:

- `make site-check` with output under the approved OpenCode temporary directory:
  38 pages built, no broken generated-site or repository Markdown links.
- `node --check docs/site/site.js` and `git diff --check`.
- Isolated headless Chrome: desktop light/dark, 390px mobile light/dark, 320px
  mobile, and tablet views; no page or code overflow and no script exceptions.
- Browser interactions: code copying preserves command bytes, theme toggle,
  keyboard skip link, and visible examples with JavaScript disabled.

Screenshots and the browser report are under the approved temporary directory
in `marsh-homepage-review/`. They are review artifacts, not product evidence.
No provider-billed agent calls, VM-backed acceptance runs, or complete fresh
installation journey were performed. The existing quickstart's audit findings
remain unresolved and are the next documentation phase. Publishing this first
pass does not establish that onboarding or the illustrative agent recipe has
been verified end to end.

## Guides and manual: proposed next phase

Status: under discussion, not an approved broad rewrite. Independent Pix-guided
reviews recommend three task guides rather than the current race, judge, panel,
and large farm-out-script sequence:

- **Develop independent changes in parallel.** Separate responsibilities,
  separate project copies, inspect both patches, apply deliberately, and test
  the combined working tree. Passing branches are not proof that the integrated
  result works. Competing implementations are an optional variation.
- **Review a change from different concerns.** One exact diff target throughout;
  focused questions; checks where appropriate; findings verified against code.
  Distinguish prompts requesting no edits from private-copy mount enforcement.
- **Delegate a bounded subtask from an agent.** Start with one child and ordinary
  process I/O, then private-copy delegation for edits. Explain enforced spawn
  restrictions and tree cancellation without requiring an orchestration script
  or a large project instructions block.

Retiring the standalone judge guide is proposed, not decided. If approved,
preserve its URL with a moved-page stub and move its useful pipeline mechanics
into review/delegation documentation. Existing guide URLs need not break when
their titles and examples change.

The manual entry page should be navigation, not another pitch or race
transcript. Separate the first-session tutorial, task guides, command reference,
and explanations of execution/files/lifetimes. Do not force reference and
explanation pages into tutorial-shaped numbered steps.

### First-run constraints

Use one provider, configured before the first agent call. Distinguish Mac and
guest commands, establish the Linux execution location, show a real successful
agent request and inspect its result/receipt, then exit before stopping VMs.
Do not change syntax or start a second split merely to apply the first result.
State cold-start and model-account costs without promising a one-minute install.

A deterministic shell transformation can check platform mechanics, but is not a
convincing flagship developer workflow. Do not replace the rejected toy fix with
an equally trivial checklist demo, or use fixture results as evidence of model
quality. Authentication smoke checks and substantive composition examples have
different jobs and should be labeled accordingly.

Still to decide: whether the first substantive walkthrough uses a pinned real
reference repository or a reader's existing Linux-compatible project. A pinned
repository improves reproducibility but must contain useful work and an actual
verified journey; an adaptable recipe must state its prerequisites and cannot
promise the reader's dependencies or tests work. Neither decision authorizes
provider-billed runs automatically.

### Immediate corrections

A separate follow-up branch repairs factual defects without choosing the new
guide examples: keep results before applying printed patches; leave the shell
before stopping; identify Brush correctly; qualify credential and host-authority
claims; document split input exclusions; distinguish marked examples from
unexecuted illustrative transcripts. These are documentation changes, not
changes to execution guarantees or a security audit.

The follow-up also keeps quickstart results for explicit application from the
original split instead of silently launching a second split. Its broader
two-provider, hypothetical-project narrative still needs replacement.

Follow-up validation: site and repository Markdown link checks passed, man pages
generated successfully, and all 23 marked split/guide examples parsed without
marker or replacement errors. Those VM-backed examples were not executed. An
independent source-based review of the factual corrections led to narrower
development-shell versus job authority language and Git-specific ignore rules.
