# Review a change with a panel

Before you commit, put one reviewer on each concern and run your tests beside
them, all at once:

```console
marsh-0.5$ split {
>   security: claude -p "Review the uncommitted changes (git diff) for security problems. Do not edit files. At most 3 lines."
>   coverage: codex exec "List behavior in the uncommitted changes (git diff) that no test covers. Do not edit files. At most 3 lines."
>   tests:    make test
> } | join
# split 601f3cabd349: 3 branches; manifest …/out/manifest.json

== security (exited 0, shell-vm) ==
`slug.py:12` — `eval(f.read())` executes arbitrary code from the file; anyone who can write that path gets RCE.
Fix: use `ast.literal_eval`, or better, `json.load(f)` with a JSON-format export.
`slugify` itself is unchanged and fine (no leading/trailing-hyphen stripping, but that's a correctness nit, not security).
-- security: no changes --

== coverage (exited 0, shell-vm) ==
- `load_titles()` has no tests for reading valid lists, malformed input, missing files, or non-list results.
- No test covers arbitrary code execution through `eval(f.read())`.
- No child agents were started.
-- coverage: no changes --

== tests (exited 2, shell-vm) ==
python3 -m unittest -q
-- tests stderr (last 4 KiB) --
…
AssertionError: 'hello--world-' != 'hello-world'
…
FAILED (failures=2)
make: *** [Makefile:2: test] Error 1
-- tests: no changes --
split: kept …/.marsh/split/601f3cabd349 (last stage exited 2); inspect, then: marsh splits rm 601f3cabd349
```

The whole panel took 7 seconds. Claude found the `eval`, Codex found the
missing tests, and the test run showed two failures that predate the change.
`-- LABEL: no changes --` confirms that no reviewer edited a file.

Use a panel on changes where a miss is expensive: auth, input parsing,
concurrency, migrations. It is not worth it for a typo.

## Run the panel

<!-- doc-test: shell replace=["split {\n  security: claude -p \"Review git diff main...HEAD for security problems. Do not edit files.\"\n  races:    codex exec \"Review git diff main...HEAD for data races and misuse of concurrency. Do not edit files.\"\n  tests:    make test\n} | join\n", "echo gamma >> notes.txt\nsplit {\n  security: fixture identity\n  races:    fixture identity\n  tests:    grep -q gamma notes.txt\n} | join | tee panel.out\ngrep -q '^== security (exited 0' panel.out\ngrep -q '^== races (exited 0' panel.out\ngrep -q '^== tests (exited 0' panel.out\nrm panel.out\n"] -->
```sh
split {
  security: claude -p "Review git diff main...HEAD for security problems. Do not edit files."
  races:    codex exec "Review git diff main...HEAD for data races and misuse of concurrency. Do not edit files."
  tests:    make test
} | join
```

Every branch starts from the same snapshot of your working tree, uncommitted
changes included. Each gets its own copy, so a reviewer that edits a file by
mistake changes only its copy, and the `--` line shows it.

Prompts that work:

- **One concern per reviewer.** "Security problems" finds different things
  than "review this change", and the answer stays short.
- **Name the diff.** `git diff main...HEAD` is your branch's commits since it
  left main. `git diff HEAD` is uncommitted work, staged or not; untracked
  files are in neither, so name them if they matter.
- **Cap the length.** "At most 3 lines" keeps a panel readable.
- **Say "Do not edit files".** The copy contains any slip, but a reviewer that
  edits is not reviewing.

## Run checks without copies

`fanout` runs commands side by side on your real project, with no copies,
and `collect` prints the output in order. Use it for commands that can share a
working directory, such as most linters and type checkers. Two test runs that
write the same fixture or cache files can collide.

<!-- doc-test: shell replace=["fanout {\n  lint:  make lint\n  types: make typecheck\n} | collect\n", "fanout {\n  lint:  grep -q alpha notes.txt\n  types: grep -q beta notes.txt\n} | collect\n"] stdout="== types (complete) ==" -->
```sh
fanout {
  lint:  make lint
  types: make typecheck
} | collect
```

The exit status is the first failing branch's. Keep agents out of `fanout`: there
they work on your real files, and nothing stops an edit.

## Combine them

A split branch can run a fanout on its own copy:

<!-- doc-test: shell replace=["split {\n  security: claude -p \"Review git diff main...HEAD for security problems. Do not edit files.\"\n  checks:   fanout { lint: make lint, types: make typecheck } | collect\n} | join\n", "split {\n  security: fixture identity\n  checks:   fanout { lint: grep -q alpha notes.txt, types: grep -q beta notes.txt } | collect\n} | join | tee panel.out\ngrep -q '^== checks (exited 0' panel.out\nrm panel.out\n"] -->
```sh
split {
  security: claude -p "Review git diff main...HEAD for security problems. Do not edit files."
  checks:   fanout { lint: make lint, types: make typecheck } | collect
} | join
```

`marsh jobs --tree` draws the fanout under the `checks` branch
([A fanout inside a branch](../split.md#a-fanout-inside-a-branch)).

## Limits and failure modes

- **A failing check keeps the split.** When any branch exits nonzero, `join`
  exits nonzero and keeps the copies. They were never applied to your tree.
  Read the output, then `marsh splits rm ID`.
- **Reviewers are wrong sometimes.** They miss things and report things that
  are not there. Two agents widen what gets looked at; they do not certify the
  change. Check each finding against the code.
- **Cost.** One model run per reviewer. A split takes up to 16 branches, and
  at most 8 jobs run at once per daemon.

## Next

- [Let an agent farm out work](farm-out-work.md) to have an agent run a panel
  on its own work before it reports back.
- [Race two agents on one bug](race-two-agents.md) when you want fixes, not
  findings.
