# Site and documentation editorial brief

The user approved a complete documentation rewrite after reviewing the homepage
on October 13, 2026. This includes checking every page and its examples, then
publishing the result. It does not authorize product changes or edits to the
separate benchmark and CI work. Leave `profiles.md` alone.

## Product story

Keep **A shell for composing agents.** marsh is a platform, not a racing tool or
an application for one outcome. Show agents used as commands at a shell prompt,
including from inside another agent's job.

Filesystem branching belongs in that explanation. A split starts with the
current project files, gives branches private copies, and returns patches for
the caller to apply. Warm VMs, copy-on-write cloning where supported, and
concurrent branches explain the speed. Do not invent latency figures or imply
that marsh speeds up model inference.

The homepage session is an example, labeled in the terminal bar. It has no
invented agent answers, successful test output, measured duration, or realistic
but fabricated split ID. Keep the shell replay and file diagram together on
mobile. The full transcript must remain readable during playback, with Stop,
Replay, reduced-motion, and no-JavaScript support.

## Writing

Write for developers who know how to use a shell. Explain what marsh adds and
what they need to do next. Do not explain `&&` repeatedly or turn every page
into another product pitch.

Read each sentence aloud. Rewrite anything that sounds like a checklist pasted
into prose: clipped noun slogans, repeated contrasts, ceremonial introductions,
or caveats that belong on a different page. A banned-word scan is not an
editorial review. Technical reference still needs exact flags, limits, defaults,
and failure behavior.

Use the Pix writing-voice, anti-slop, DevRel, DX, and verification guidance.
Gemini 4 Argon provides the adversarial editorial review. Its suggestions need
the same scrutiny as the draft: an invented example ID is still invented, and
removing a necessary qualification can make a shorter sentence false.

The requested review lenses are editorial questions, not attributed opinions:

- Patrick Collison: what does this sentence tell the reader that they need?
- Tobi Lütke: are we making the work harder by explaining the machinery?
- Solomon Hykes: can a reader see useful combinations of a few shell primitives?
- Mitchell Hashimoto: can they predict execution location and file access?
- Simon Willison: can they reproduce the example and inspect its evidence?

## Documentation and examples

The first run uses one provider, configured before the first agent request. It
works with the reader's existing repository rather than an imaginary failing
test. Separate Mac-terminal commands from guest-shell commands, and leave the
shell before stopping VMs.

The guides cover independent changes, review, and bounded delegation. The old
judge URL remains useful for optional downstream review; no model becomes an
approval authority. Preserve existing URLs and repair any changed links.

Treat examples according to what they are. Execute marked command blocks in
isolated scopes. For model-backed recipes, declared fixture substitutions check
the shell composition and patch handling, not the model's judgment. Test ACP and
MCP through their real callers with deterministic agents. Exercise installation
in a disposable prefix rather than overwriting the user's install.

Keep a verification inventory that distinguishes executed examples, adaptable
project recipes, output excerpts, configuration samples, and host-mutating
instructions. Do not call an unexecuted transcript a passing test. Record the
shipping binary and fixture identities used in the runs.

Design documents must distinguish shipped behavior from historical proposals.
Preserve formal invariants and source mappings; changing prose is not evidence
that the Rust implementation satisfies the model.

## Publication history

PRs #77 and #78 introduced the first homepage and factual repairs. The later
benchmark merge, #79, reverted those files; #80 restored them without removing
benchmark work. PR #81 restored the short replay and explained file branching
and speed. The user then rejected the remaining generated voice and authorized
this full editorial pass.

Work from an isolated worktree based on current main. Verify the PR's changed
files before merging. After Pages finishes, inspect the public HTML and assets;
a successful deployment alone does not establish that the right page shipped.
