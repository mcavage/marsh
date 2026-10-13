# Moderated sessions: five developers meet marsh

This is a script for one recorded 90-minute session with each of five
developers, plus one pilot session that is not counted. The moderator reads
the plain text. Notes in *italics* are not read aloud. Use the
[observer sheet](OBSERVER_SHEET.md) during the session and the
[synthesis template](SYNTHESIS_TEMPLATE.md) after all five.

## What this is, and is not

This is **one diagnostic round**. It is meant to find serious usability
problems and wrong mental models early. It is not a release gate. It cannot
estimate how often a problem happens, because each participant moves a
percentage by 20 points. Five sessions will miss problems that are rare or
specific to one kind of user. It also cannot show that marsh is safe: no
incident in five sessions is weak evidence. Report counts and each
participant's path, not rates. Follow up with a second round after fixes, and
test safety separately.

## Goals

1. Can a developer new to marsh get from nothing to a passing race with the
   public docs and whatever assistant they normally use, and where do they
   stall? Results are reported by the source they used (docs, assistant,
   `--help`).
2. What do they believe marsh changes on their machine and in their repo
   (forks, patches, `--keep`, VMs)? A wrong belief here is the most costly
   finding, so it gets protected time (part 6).
3. Can they tell what ran, and clean up?

## Participants

- Five professional developers who use a coding agent (Claude Code, Codex, or
  similar) at least weekly and have never used marsh.
- An Apple Silicon Mac they use every day, on macOS 26 or later, with
  Homebrew. Their own machine shows real friction. Infrastructure failures are
  classified separately (see "Error categories").
- A mix: at least two who use a shell all day, at least one who mostly works
  in an IDE, and at least one who has used Docker Sandboxes or devcontainers.
  Exclude anyone who works on marsh or has read its design documents.
- Compensation is agreed in advance and does not depend on finishing.

## Frozen for the round

Record these on the observer sheet. Do not change them mid-round.

- **marsh and sbx versions:** the current stable release on the first
  session's date (see "Window").
- **Docs snapshot:** the date and commit of runmar.sh. Tell the team not to
  deploy the site during the round.
- **Practice repo:** a fixed commit of a small **Python** project. It has one
  failing test from an off-by-one bug, a `make test` target, and a README. The
  moderator also has a **held-out check**, `validate.sh`, kept outside the
  repo. It runs the visible test plus three regression tests and fails on a
  patch that edits or deletes the test. Before the round, in marsh's shell VM,
  confirm three things: `make test` fails at the commit; the known-good fix
  passes both `make test` and `validate.sh`; and a test-deleting "fix" fails
  `validate.sh`.
- **Credentials:** the team provides disposable keys for Anthropic and OpenAI,
  one pair per participant, each with a hard $10 spend cap. Revoke them after
  the session. Participants never use their own keys. That keeps provider keys
  off screen, but not other secrets, so see "Secret handling".
- **Window:** run all five sessions within one week, agreed with the release
  owner so no marsh release ships in that window. Serve the docs from a frozen
  snapshot or deploy freeze, so Homebrew and install-script users get the same
  version.

## Before each session

- [ ] Consent form signed. It covers screen, audio, and the terminal log;
      face video only if the participant opts in; who can see the raw files;
      deletion after 90 days; and the right to stop at any time.
- [ ] 48 hours ahead, send: "Please make sure Homebrew works and you have about
      5 GB free. Don't install anything else, and please don't look marsh up
      before the session."
- [ ] Recording: a video call with screen share and local recording. Once
      marsh is installed, also record the participant's terminal with
      `script -r ~/marsh-session.typescript` (ask first). Video shows
      behaviour; the typescript holds exact commands.
- [ ] Inventory **before**, with the participant's permission, saved to the
      observer sheet: `brew list --versions | grep -Ei 'sbx|marsh'`,
      `ls -d ~/.marsh ~/.sbx 2>/dev/null`, `command -v sbx marsh`.
- [ ] A second person observes with the observer sheet, camera and mic off.
- [ ] Do not install marsh or sbx for them. Installation is task 1.

**Secret handling.** Rehearse this in the pilot:

- Before recording starts, the participant turns on Do Not Disturb, opens a
  **new terminal window with a fresh profile** (no history, no prompt that
  shows a hostname or branch they mind sharing), and uses a browser profile
  with no signed-in sessions. Screen-share only that terminal and that browser
  window.
- Keys go through a one-time secret link (a password manager share), sent
  outside the recorded call, and are typed only at an `sbx secret set`
  prompt. Never paste keys into the call's chat.
- Pausing the call pauses neither `script` nor the screen recorder. If
  something private appears, say "pause", stop the screen share, and the
  moderator notes the minute for redaction.
- Afterwards, the moderator reviews the typescript and the video at the noted
  minutes, and searches the typescript for key prefixes (`sk-`) and the
  participant's username. Hits are redacted in a working copy. The raw files
  stay in a restricted folder and are deleted after 90 days. A key that was
  exposed is revoked at once; a personal secret that was exposed is reported
  to the participant the same day.

## Session outline (90 minutes)

| Min | Part |
|---|---|
| 0–5 | Welcome, consent, think-aloud practice |
| 5–10 | Background questions |
| 10–28 | Task 1: install |
| 28–38 | Task 2: first agent call |
| 38–60 | Task 3: race two agents on the bug and apply one |
| 60–67 | Task 4: what happened? |
| 67–75 | Part 6: what changed where (protected; never cut) |
| 75–78 | Task 5: clean up |
| 78–90 | Debrief and questionnaire |

Time limits are hard. When one runs out, stop the task, record "not
finished", and continue. Do not fold the overrun into a completion time.

## 0–5: Welcome

> Thanks for doing this. We're testing marsh, not you. If something is
> confusing, that's exactly what we need to find. Please think aloud: say what
> you're looking for, what you expect, and what surprises you, even small
> things. Please be blunt. I'll mostly stay quiet; if you get stuck I may ask
> what you'd do next. We're recording your screen and voice, and later your
> terminal. I'll give you API keys for this session when you need them; they
> stop working afterwards.

Think-aloud warm-up (one minute): "To practise, open your terminal and find
out how much disk space is free, talking me through it."

## 5–10: Background

1. What do you use coding agents for in a typical week? Which ones?
2. Have you ever run two agents on the same problem? How did you choose
   between their results?
3. When an agent changes files in your repo, how do you check what it did?
4. Have you used Docker, Docker Sandboxes, devcontainers, or VMs for
   development? How do you feel about them?

## Task 1: install (limit 18 minutes)

> Here's the only thing you get: the website, **runmar.sh**. Get marsh
> installed so you could use it on a project. Use any browser, search, or
> assistant you'd normally use.

*Do not point to the install page. A participant who asks an AI assistant may
do so; record which assistant and what it said. This connects to the reach
eval.*

Observe and time (see "Timing"):

- Time to the first install command, and until `marsh --version` works.
- Did they discover that Docker Sandboxes (`sbx`) is needed, and `sbx login`?
- Did they store credentials (`sbx secret set`) now, or only after a failure?
  *(Paste the keys when they reach that step, or when an auth error appears.)*
- Homebrew tap or install script, and why.
- Reactions to unsigned binaries, VM downloads, or disk use.

**Standard rescue** (up to 7 extra minutes, taken from task 4). If install is
not done at the limit, the moderator reads out the four commands from the
install page and the participant types them. If that fails too (network, sbx
service, virtualization), switch to the **spare Mac**, a team machine prepared
with the frozen versions and the practice repo, and continue there. Every
later task is then labelled **onboarding bypassed**, and with the spare Mac,
**researcher environment**. Those tasks still give diagnostic evidence, but
they never count as independent end-to-end success. If task 2 fails because
the agent cannot start, use the same ladder: read out the fix, then switch to
the spare Mac.

## Task 2: first agent call (limit 10 minutes)

> Clone this repo *(share the URL)* and go into it. Using marsh, ask Claude
> what the project does.

**Before they run anything**, ask once: "What do you expect to happen when you
run that?" Write the answer down verbatim. Do not ask where it runs; anything
they say about location here is spontaneous.

Observe:

- Do they open the marsh shell (`marsh`) or use `marsh -c`?
- The first Kit VM boot (`[starting claude worker VM…]`): how long it takes,
  what they think is happening, and whether they think it's stuck.
- An auth failure, if they skipped credentials: does the error tell them how
  to fix it?

Success: Claude's answer appears in their terminal, from a marsh-run Claude.

## Task 3: race two agents and apply one (limit 22 minutes)

> `make test` fails in this repo. Use marsh to have **two different agents try
> to fix it at the same time**. Then put the fix you prefer into your repo.

*The wording deliberately says nothing about isolation, copies, or safety, so
any isolation they choose is their own idea. Do not say "split", "join",
"fork", "fanout", or "race". If they ask whether marsh can do this, answer:
"What would you look for to find out?"*

**Before they run their first marsh command for this task**, ask once: "What
do you expect to happen when you run this?" Record it verbatim.

*Observer: the repo must be clean at the start of the task. Just before it, the
moderator records, in a second terminal, a content snapshot of every file,
including ignored ones:
`find . -path ./.git -prune -o -path ./.marsh -prune -o -type f -print0 | xargs -0 shasum > /tmp/before.sha`.
If `fswatch` is installed, also run `fswatch -r . > /tmp/writes.log` in the
background. Repeat the snapshot, without comment, after the agents finish
and before anything is applied. Report the result as "no observed change"
(and, with fswatch, "no observed writes"), not as proven isolation.*

Observe:

- Where they look first: README, quickstart, guides, `marsh --help`, `man
  marsh`, or an assistant.
- `split` (private copies) or `fanout` (shared files)? If fanout, did agents
  write the real files, and did the participant notice?
- Do they gate each branch on `make test` unprompted?
- Reading `join` output: which branch passed, what files changed, where the
  patch is.
- Do they notice an agent editing the test file?
- How they apply it (`git apply …/diff.patch`, `join -- …`, or by hand), and
  which branch's patch it was (**provenance**). Do they run `marsh splits rm`
  or understand `--keep`?

After the task, the moderator runs `validate.sh` in the participant's repo.

Score five things separately (observer sheet):

1. **Isolation:** no observed change to the real files before the apply.
   This is a safety observation, not part of task success, since the task
   did not ask for isolation.
2. **Execution:** both agents ran.
3. **Interpretation:** they correctly said which branch passed and what it
   changed.
4. **Application:** a patch was applied, and they named its branch correctly.
5. **Correctness:** `validate.sh` passes.

Task success means execution, interpretation, application, and correctness,
at help level ≤ 2 (unassisted or prompted). The triage rules use the same
definition.

## Task 4: what happened? (limit 7 minutes)

> Suppose a teammate asks what ran and how long it took. Find out.

Observe: do they find `marsh jobs --tree` and `marsh results`, and can they
read the job tree (the split, its branches, the agent jobs)? Ask them to point
to the job that took longest.

Success: they name both agents' jobs, with an exit status and duration for
each.

## Part 6: what changed where (8 minutes, protected)

Ask in this order. Record which statements were **spontaneous** (said during
the tasks, before this part) and which were **probed** (said here). The task
3 wording already mentions "at the same time", so even spontaneous answers
come from a primed setting. Assessing completely unprimed understanding
would need separate sessions.

1. "Draw or describe where your files, the shell, and each agent were during
   the last task."
2. "During the last task, which files on your Mac changed, and when?"
   *(Compare with the `git status` snapshots.)*
3. "Where is the other agent's attempt now?"
4. "What is still running, or still on disk, because of marsh?"

## Task 5: clean up (limit 3 minutes)

> You're done for the day. Leave your machine the way you'd want it.

Observe: `marsh stop`, `sbx` commands, deleting `.marsh/`, uninstalling, or
nothing.

## Help ladder

Use the lowest level that unblocks them. Log every intervention with its
level and minute.

0. Silence; wait 60 seconds.
1. A neutral prompt: "What are you trying to do right now?" / "What would you
   try next?"
2. A pointer to a place: "The docs cover that." (No page name.)
3. The page: "Look at the race guide."
4. The answer, typed by them.

Outcome classes, recorded per task alongside the highest level used:
**unassisted** (0–1), **prompted** (2), **assisted** (3–4), **not finished**,
**onboarding bypassed**.

## Think-aloud prompts (neutral)

- "What are you thinking?"
- "What did you expect to happen there?"
- "What does that output tell you?"
- "How would you know whether that worked?"

During tasks, never ask "Did you see the X?" or "Would a Y help?", and never
ask where something runs except in the two scripted prediction questions.

## Timing

From the video, record for each task: **elapsed** time; **waiting** time
(VM boot, downloads, model latency: the participant is idle and the system is
busy); and **active** time (elapsed minus waiting). Report all three, and list
unfinished tasks separately, never as times.

## Error categories

Log each error with one or more of these labels, plus its user impact
(minutes lost, gave up, worked around) and how well the participant
recovered:

- **Docs:** could not find or misread documentation.
- **Command:** wrong command or flag.
- **Concept:** wrong mental model, such as fanout for writes or applying
  the wrong patch.
- **Product:** marsh behaved badly or its message misled.
- **Infrastructure:** network, Homebrew, sbx service, provider outage, or
  quota. Record the technical cause, but keep the user impact and how
  understandable the failure was. Those are part of onboarding, so a confusing
  infrastructure error is also a product finding.
- **Expected:** a nonzero exit that is part of the task (the failing test).
  Not an error.

## Debrief (12 minutes)

1. In one sentence, what is marsh?
2. What was the most confusing moment? The most reassuring?
3. When both agents finished, how did you decide which fix to take?
4. Is there anything you would not trust marsh to do on your real repo? Why?
5. What would you use it for next week, if anything?
6. What did you expect it to do that it didn't?

Then the 10-item SUS questionnaire, standard wording with marsh as the
system, filled in by the participant.

## After the session

- Inventory **after**, with permission: the same commands as before, plus
  `marsh status` and `sbx ls`. Compare the results with the participant's
  answer to part 6, question 4.
- Offer to run `marsh stop` and to remove marsh and sbx (`docs/install.md`),
  and do only what they agree to.
- Revoke the disposable keys and record their spend.
- Redact the copies (see "Secret handling") before anyone else watches.

## What to record, per participant

- Video (face only if opted in), the terminal typescript, and the observer
  sheet.
- Versions, the docs snapshot, and before/after inventories.
- Per task: the outcome class, the highest help level and each intervention,
  elapsed/active/waiting time, and errors by category.
- Task 3's five scores, the `git status` snapshots, and the `validate.sh`
  result.
- Predictions, part 6 answers (spontaneous or probed), the drawing, debrief
  answers, and the SUS score.
- Key spend.

## Triage rules (internal; set before session 1)

These are not statistical thresholds. Any of these makes a finding a top
finding in the synthesis:

- Fewer than 4 of 5 install unassisted.
- Fewer than 3 of 5 succeed at task 3 (as defined there).
- Anyone gets isolation or provenance wrong in part 6, or applies a patch from
  a different branch than they believe.
- Any loss of work, or a real file changed during a race the participant
  thought was isolated.

Report SUS and page counts as supporting evidence only. SUS has no validated
threshold for this product.

## Pilot

Run the full protocol once with a colleague who is not on the marsh team
(P0). Fix the timings, `validate.sh`, and the secret-handling steps. P0's data
is not part of the synthesis.
