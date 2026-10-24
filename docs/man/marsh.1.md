---
title: MARSH
section: 1
date: October 13, 2026
source: marsh
manual: User Commands
---

# NAME

marsh, msh - shell for composing agents

# SYNOPSIS

**marsh** [**--shell** **bash**|**zsh**|**marsh**] [**--load** *KIT*[,*KIT*...]|**all**] [**--ephemeral-home**] [**--dev**] [*SHELL_ARGS*...]

**marsh** *COMMAND* [*ARGS*...]

**msh** ...

# DESCRIPTION

**marsh** opens a Linux shell for the current directory on an Apple Silicon
Mac. The shell is a patched Brush, a Bash-compatible shell, running in a
Docker Sandbox VM created with the stock **sbx** command. The project
directory is mounted read/write at its exact Mac path, so **pwd** prints the
same path inside and outside.

Some command names are *registered*. By default these are **claude**,
**codex**, **pi** and **shell**. Each registered command runs as a fresh,
nonroot container from its Kit image, in a warm VM kept for that Kit. The
container is deleted when the command ends.

Registered commands work in pipelines, scripts, **make**, and background jobs
like any other command. A registered command started from inside another one
becomes its child job.

**split** runs commands in parallel on private copies of the project.
**join** prints each branch's output and patch. Registered commands in a split
mount only their fork; use **join --keep** to retain patches for inspection
and application. A shell branch's own code (**-b**) is not confined to its
fork. **fanout** runs branches on shared files, and **collect** prints their
output in declaration order.

Direct jobs can change or delete project files. Jobs using the same guest
home can read one another's settings, history, and saved logins. Keep the
project in Git, and use **split** for agent changes you want to review first. See
https://runmar.sh/docs/security.html.

**msh** is a symbolic link to **marsh** and behaves identically.

With no *COMMAND*, **marsh** starts the shell. Arguments it does not recognize
go to the shell, so **marsh -c** '*command*' and **marsh** *script* work as
they do with Bash. After **-c** or **--**, marsh interprets nothing.

# OPTIONS

**--shell** **bash**|**zsh**|**marsh**
: Run the shell VM's GNU Bash or zsh, with your own startup files, instead of
  Brush (**marsh**, the default). Registered commands and the **marsh** CLI
  work in all three.

**--load** *KIT*[,*KIT*...]|**all**
: Prepare the named Kit VMs before the prompt appears. Otherwise the first call
  to a Kit boots its VM and prints **[starting** *NAME* **worker VM…]**.

**--ephemeral-home**
: Use a blank, private guest home that is not kept after the shell exits. If it
  cannot be released, the slot is retained; see **recover-home**.

**--dev**
: Open a development shell whose **sbx** is a scoped host broker. Its grant
  belongs to the shell, not Kit jobs. For developing marsh itself; requires a
  development-enabled install or daemon.

**-h**, **--help**
: Print a summary of commands and exit.

**-V**, **--version**, **version**
: Print the version and exit.

# COMMANDS

**status** [**--json**]
: Show the daemon, its host control directory, per-job limits, shells, and
  Kit workers.

**jobs** [**--all**] [**--tree**] [**--json**]
: List Kit jobs, running first, then newest. **--tree** draws children under
  their parent. In a shell, only that session's jobs; from a host terminal,
  running trees, trees from the last hour, and the five newest. **--all** lists
  every recorded job. **--json** omits the session/recent-jobs filter.
  Inside a job, all forms remain limited to that job's subtree.

**jobs show** *JOB* [**--json**]
: Show one job. *JOB* may be an id prefix.

**run** [**--spawn** *NAME*,...|**--no-spawn**] *NAME* [*ARG*...]
: Run registered command *NAME* as a new job. Inside a job it is a child job.
  **--spawn** and **--no-spawn** narrow what *NAME* may start in turn.

**context**
: Print the text every job is given about marsh (its name, limits, and how to
  start other jobs).

**results** [**--json**]
: List finished jobs, newest first: command, exit status, cleanup, timing.
  **WALL** is the whole request, from the call to verified cleanup. It
  includes any Kit VM boot. **jobs** shows **RUN**, the job's own run in the
  ready VM.

**results show** *CURSOR*|*JOB* [**--json**]
: Show one finished job.

**split** [**-n**] [**-b** *LABEL*=*STRING*]... [**:::** *LABEL* *CMD* [*ARG*...]]...
: Run each branch at once in its own private copy of the current directory and
  wait for all of them. **-b** runs *STRING* with the shell in the shell VM;
  **:::** runs one registered command in a container that sees only its copy.
  **-n** gives branches empty stdin. Prints one handle line, which **join**
  reads on stdin.

**join** [**--json**] [**--keep**] [**--timing**] [**--** *CMD* [*ARG*...]]
: Read a split handle on stdin and print each branch's status, output, and
  patch. Only the split's creator can join it. **--keep** retains the split
  and its patches, even on success; without it, success removes them.
  **--json** prints the manifest instead of the text rendering. **--timing**
  prints phase timings to stderr. With **--** *CMD*, run *CMD* on that output with **SPLIT_ID**,
  **SPLIT_DIR**, **SPLIT_MANIFEST** and **SPLIT_OBJECTS** set; the split is
  removed if *CMD* exits 0, unless **--keep** is set, and kept otherwise.
  *CMD* runs even if a branch failed, and **join** returns its status. Without
  *CMD*, **join** returns the first nonzero branch status.
  *CMD* runs in the caller's environment: on the Mac, in the shell VM, or
  in the job container where **marsh join** was invoked.

**splits** [**--json**] [*ID*]
: List splits. With *ID*, show its state and phase timings, even after joining.

**splits cancel** *ID*
: Cancel a running split.

**splits rm** *ID*
: Remove a kept split and its patches.

**fanout** [**-n**] [**-b** *LABEL*=*STRING*]... [**:::** *LABEL* *CMD* [*ARG*...]]... | **collect** [**--json**] [**--timing**] [**--stderr**]
: Run branches at once on the same files (no copies). Print each branch's
  output in declaration order: its header, its stdout, then a failed branch's
  stderr. **--stderr** prints every branch's stderr. **collect** exits with
  the first nonzero branch status. Limits: 16 branches, 64 MiB of input, 16 MiB
  of combined branch output.

**cloud** [**--cpus** *N*] [**--ttl** *DURATION*] [**--image** *REF*] [**-d**] [**-c** *SCRIPT* [*NAME* [*ARG*...]] | **--** *PROGRAM* [*ARG*...]]
: Preview. Run a script or program in a new Docker Cloud sandbox; in the
  marsh shell, **cloud** [*OPTION*...] **{** *LIST*; **}** does the same. With
  no command, from a terminal, open an interactive cloud shell. A private,
  hook-free copy of the project's Git metadata goes with it and is never
  written back. The project (or split
  fork) is uploaded without Git-ignored files, **.git**, or **.marsh**; stdin,
  stdout, stderr, and the exit status pass through; file changes are written
  back to it before the command returns, overwriting local edits to the same
  paths. Needs a Docker sign-in with Docker Cloud sandbox access. Ctrl-C exits
  130 after the remote command stops and its changes return. See
  [Docker Cloud](../cloud.md).

  With **-d** (**--detach**) the run is detached: marsh prints its job number
  and returns at once, and the run continues with the laptop closed (no
  terminal, no input; **--ttl** defaults to 2h and pauses the run rather than
  ending it). marsh collects it by itself and writes its changes back only if
  none of the files it changed were edited here meanwhile; otherwise they are
  kept as a patch. A detached block that calls a registered Kit is refused.

**cloud attach**|**collect**|**diff**|**apply**|**discard**|**stop** *JOB*
: Control a detached cloud run: **attach** shows its output from the start and
  follows it (Ctrl-C stops following, not the run); **collect** checks on it
  now; **diff** shows a kept patch; **apply** applies a kept patch anyway;
  **discard** deletes its sandbox and drops any kept patch; **stop** sends INT,
  then KILL after 10 seconds.

**config shell** [**bash**|**zsh**|**marsh**]
: Set, or with no name print, the default shell for this **MARSH_HOME**.

**config auto-update** [**on**|**off**]
: Set, or with no value print, whether Kits that follow a tag update
  themselves in this **MARSH_HOME** (when the daemon starts, then hourly), and
  whether a release build adds **pix** and **hermes** by itself. The default
  is **on**. To update now, run **marsh stop** when idle, then marsh again.

**config open-urls** [**on**|**off**]
: Set, or with no value print, whether **xdg-open** *URL* (also **open**,
  **marsh open**) in the shell VM or a job started from your terminal opens an
  **http**/**https** link in your Mac's browser. Off, links are only printed.
  The default is **on**. See
  https://runmar.sh/docs/agents.html#opening-links.

**kit install** *NAME* [*REPOSITORY*:*TAG*|*REPOSITORY*@sha256:*DIGEST*]
: Register a published Kit under *NAME* in this scope. A tag is resolved to its
  digest now; only that digest runs. Without a reference, *NAME* must be a Kit
  marsh knows by name: **hermes** or **pix**.

**kit uninstall** *NAME*
: Remove a Kit added with **kit install** (a removed **pix** or **hermes**
  is not added again until installed), or return an updated packaged command
  to the version this marsh shipped.

**workers reset** *KIT*[,*KIT*...]|**all**
: Remove idle Kit VMs, including quarantined ones. The next use creates a new
  VM. **workers reset cloud** deletes every Docker Cloud sandbox this daemon
  owns and verifies they are gone, except detached runs' sandboxes; with
  **--detached** those runs are discarded too.

**reset** [**--json**] [**--detached**]
: Remove every VM this scope owns (detached cloud runs are kept unless
  **--detached** is given). The daemon keeps running. Refuses while
  shells or jobs are active. The guest home and results are kept. Exits
  nonzero if any cleanup could not be verified.

**stop** [**--json**] [**--detached**]
: Remove every VM this scope owns (detached cloud runs are kept, and collected
  when the daemon starts again, unless **--detached** is given) and stop its
  daemon. Refuses while shells or
  jobs are active. The guest home and results are kept. Exits nonzero if any
  cleanup could not be verified.

**recover-home** *SLOT* **--discard**
: Discard a retained **--ephemeral-home** slot (0 to 15).

**mcp install** **codex**|**claude**|**sbx**
: Register marsh as an MCP server for this project. **codex** and **claude**
  launch it on demand; **sbx** gives selected sandboxes a shared per-project
  host broker. Run this on the Mac. The server uses a separate managed scope
  root rather than the CLI's default home.

**mcp install-published codex** *NAME*, **mcp remove-published codex** *NAME*
: Register or remove a published pipeline as a tool for host Codex. Only
  **codex** is supported. Run on the Mac from the publishing project with the
  same **MARSH_HOME**. Removing the registration does not revoke the publication.

**mcp serve** [*MCP_OPTIONS*...]
: Serve MCP over stdio from a host process. Without **--scope-root** or
  **--home**, use the CLI's selected home and disable creation of managed scopes.

**mcp export-serve** [*MCP_OPTIONS*...] **--declaration** *PATH*
: Serve only the tools in a publication declaration. **--tool** *NAME* selects
  one operation from a bundle. Requires **--home**.

**mcp start**, **mcp stop**
: Start or stop the project's host MCP broker. **stop** refuses while a sandbox
  is attached.

**mcp load** *NAME* **--sandbox** *SANDBOX*
: Load an existing publication into a running same-user sandbox. Does not
  replace the publication or invalidate other clients.

**mcp unpublish** *NAME*
: Revoke a publication, including calls from clients that already loaded it.

**acp install-published codex** *NAME*, **acp remove-published codex** *NAME*
: Register or remove a published ACP control tool with host Codex. Only
  **codex** is supported. Run on the Mac from the publishing project with the
  same **MARSH_HOME**.

**help** [*COMMAND*]
: Show general help or help for a host command that has a help section.

# MCP SERVER OPTIONS

**--workspace** *PATH*
: Manage this project directory. Defaults to the current directory.

**--scope-root** *PATH*
: Use a private root for separate managed scopes.

**--home** *PATH*
: Use one **MARSH_HOME**, with managed-scope creation disabled. Cannot be
  combined with **--scope-root**.

**--marsh** *PATH*, **--sbx** *PATH*
: Select the installed marsh and stock Docker Sandboxes executables.

**--allow-full-sbx-control**
: Enable fixed repository qualification gates on the Mac. These execute the
  project's Makefile; use only with a trusted checkout. Does not enable a
  generic host-command tool.

# SHELL COMMANDS

Inside the shell, these commands are also available:

**ps --marsh** [**--verbose**|**--json**], **top --marsh** [**--verbose**] [**--once**]
: Show this project's shell, Kit jobs, and ACP sessions. **top --marsh** cannot show
  CPU or memory.

**acp reserve** *AGENT*
: Reserve a session ID for this shell and print it. Start the session within
  15 seconds or reserve again.

**acp run** [**--reservation** *ID*] *AGENT* **&**
: Start an agent session as a background Kit job. Print its session and job
  IDs to stderr when ready.

**acp list** [**--mine**] [**--wait** [*ID*]] [**--json**]
: List this project's sessions. **--mine** selects those this shell started.
  **--wait** waits at most four minutes for readiness; without an ID it
  requires exactly one live session. Use a reserved ID to wait for a new one.

**acp ask** *ID* *TEXT*
: Send a prompt and print the answer. A sole **-** reads UTF-8 stdin.
  Text and its encoded control messages must each fit 1 MiB. After an
  uncertain failure, inspect **acp status** before retrying through
  **acp prompt --key** with the reported key and identical text.

**acp prompt** [**--key** *UUID*] *ID* *TEXT*
: Submit a prompt without waiting for the answer; **-** reads stdin as for
  **ask**. Use **status** to read updates. Reuse a key only with identical text.

**acp status** *ID* [*CURSOR*] [**--json**]
: Show state, agent updates, and pending permissions. Pass the JSON reply's
  **next_cursor** back unchanged to read only newer updates.

**acp permissions** *ID* [**--json**]
: List requests awaiting one-time approval and the options for answering them.

**acp respond** *ID* *REQUEST* *OPTION*
: Answer a pending permission request with one of its offered options.

**acp cancel** *ID*
: Cancel the current turn without stopping the session.

**acp attach** *ID*, **acp release** *ID*
: Take control of an unowned session or release this shell's control.

**acp stop** *ID*
: Stop the session and its Kit job.

**acp publish** *ID* **--name** *NAME* [**--kit** *KIT*|**--sandbox** *SANDBOX*]
: Publish an idle session as an MCP control tool. Callers can prompt, cancel,
  and answer one-time permissions. With a target, load it there immediately;
  start a new client agent session to discover it.

**acp unpublish** *NAME*
: Revoke the control tool. Unpublish before steering the agent from its shell.

ACP usage errors exit 2. Other failures print **acp:** *MESSAGE* and exit 125.
See https://runmar.sh/docs/acp.html.

**mcp publish** *NAME* [**--description** *TEXT*] [**--kit** *KIT*|**--sandbox** *SANDBOX*] **--** '*PIPELINE*'
: Publish a fixed pipeline as an MCP tool. With **--kit** or **--sandbox** it is
  loaded there now. With neither, every agent Kit VM created from now on loads
  it before its first job. Kit VMs already running are not changed; use
  **mcp load** *NAME* **--kit** *KIT* to load into one now.

**mcp load** *NAME* **--kit** *KIT*|**--sandbox** *SANDBOX*
: Load an existing publication into the named Kit VM or running sandbox.

**mcp unpublish** *NAME*
: Revoke the tool everywhere it was loaded.

**split {** *label*: *commands* ... **} | join** [**--keep**] [| *CMD*], **fanout {** ... **} | collect**
: Brush forms of **split** and **fanout**.

# ENVIRONMENT

**MARSH_HOME**
: Absolute path of the scope root. The guest home is *$MARSH_HOME/home*.
  Default *~/.marsh*.

**MARSH_SHELL**
: The shell to run (**bash**, **zsh**, or **marsh**). **--shell** overrides it.

**MARSH_SBX**
: Path to the stock **sbx** executable. Default: **sbx** on **PATH**. If
  there is none, install Docker Sandboxes with
  **brew install docker/tap/sbx**, then run **sbx login**.

**MARSH_SPAWN**
: Comma-separated registered names that jobs started from this shell may
  start, or **none**. A job cannot widen the list.

**MARSH_CONTROL_HOME**
: Host-only state root: scope control directories and MCP/ACP publications
  live under it. Default: control directories in
  *~/Library/Application Support/marsh/control*, publications in
  *~/Library/Application Support/marsh*.

**MARSH_JOB_CPU_MILLIS**, **MARSH_JOB_MEMORY_BYTES**, **MARSH_JOB_PIDS**, **MARSH_JOB_WRITABLE_BYTES**, **MARSH_JOB_OUTPUT_BYTES**, **MARSH_JOB_WALL_SECONDS**, **MARSH_TREE_KIT_VMS**
: Per-job limits, read when the daemon starts.

**MARSH_PLACE**
: Placement for registered commands. Only **local** is accepted.

**MARSH_AUTO_UPDATE**
: **0** (or **off**) when the daemon starts turns off automatic Kit
  updates and the adding of **pix** and **hermes**, as
  **marsh config auto-update off** does.

**MARSH_OPEN_URLS**
: **0** (or **off**) when the daemon starts stops **xdg-open** in a shell or
  job from opening links on the Mac, as **marsh config open-urls off** does.

**MARSH_OPEN_COMMAND**
: The program the daemon runs as *PROGRAM* *URL* to open a link, read when
  the daemon starts. Default */usr/bin/open*.

# FILES

*$MARSH_HOME/home* (default *~/.marsh/home*)
: Backing store for the guest home.

*$MARSH_HOME/config.json* (default *~/.marsh/config.json*)
: The default shell set by **marsh config shell**, and the
  **marsh config auto-update** and **marsh config open-urls** settings.

*$MARSH_HOME/sbxenv.yaml*, *$MARSH_HOME/sbxenvs/NAME.yaml*
: Optional shared and per-command MCP servers and extra directories. Read at
  daemon startup; changes require **marsh stop** and a new launch.
  See https://runmar.sh/docs/configuration.html#servers-and-extra-directories.

*~/Library/Application Support/marsh/control/HASH/*
: Host-only control directory for a scope: **commands.json**,
  **agents.json**, the results journal. **marsh status** prints its path.

*PREFIX/libexec/marsh/*
: Linux guest binaries, the packaged command registry, and the shell image
  reference.

*.marsh/split/ID/out/*
: Results of a split, inside the project.

# EXIT STATUS

As for Bash, with these additions:

0-255
: Registered commands return the container's exit status.

125
: A job start was refused. The reason is on stderr. **acp** failures also exit
  125.

126
: A registered-name link was refused because **MARSH_SPAWN** excludes the
  name. **marsh run** returns 125 for the same refusal.

130
: The job was interrupted.

# EXAMPLES

Start a shell in a project:

    cd ~/src/project && marsh

Ask two agents for independent fixes in private copies and look at both:

    marsh split -n ::: a claude -p 'fix the flaky test' \
                   ::: b pi -p 'fix the flaky test' | marsh join --keep

**join** prints, for each branch, a header, its output, and a line giving the
files changed and the path of its **diff.patch**. **--keep** retains the patches
for inspection and **git apply**; a successful bare **join** removes them.
After applying a patch, remove the retained split with **marsh splits rm** *ID*.
Sample output: https://runmar.sh/docs/split.html.

Talk to one agent across turns, inside the shell:

    id=$(acp reserve pi-session)
    acp run --reservation "$id" pi-session &
    acp list --mine --wait "$id"
    acp ask "$id" 'What project files can you see? Do not change files.'
    acp ask "$id" 'Which of those describe how to run the tests? Do not change files.'
    acp stop "$id"

Run tests and lint at once:

    marsh fanout -n -b test='make test' -b lint='make lint' | marsh collect

Publish a pipeline in the shell, then give it to Codex on the Mac:

    mcp publish lint -- 'make lint 2>&1'

From a Mac terminal in the same project and **MARSH_HOME**:

    marsh mcp install-published codex lint

# SEE ALSO

**sbx**(1), **bash**(1)

Documentation: https://runmar.sh/docs/ (start with
https://runmar.sh/docs/quickstart.html; limits and trust in
https://runmar.sh/docs/security.html)

# HISTORY

marsh began life as the mark shell.

# BUGS

Report bugs at https://github.com/mcavage/marsh/issues.
