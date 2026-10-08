---
title: MARSH
section: 1
date: October 7, 2026
source: marsh
manual: User Commands
---

# NAME

marsh, msh - Bash with split, fanout, join, and collect for running agents in parallel, each command in its own sandbox

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

**split** runs commands in parallel, each on its own private copy of the
project, and **join** prints what each said and a patch of what it changed.
An agent in a split branch, whether a **:::** branch or a command started from
a shell branch, mounts only its fork; you apply its patch. A shell branch's own
shell code (**-b**) is not confined.

marsh does not sandbox the project from its commands. Every job can read and
write the project directory and the guest home. Keep the project in Git, and
use **split** to review changes before applying them. See
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
: Open a development shell whose **sbx** is a confined broker on the host. For
  developing marsh itself; requires a development install.

**-h**, **--help**
: Print a summary of commands and exit.

**-V**, **--version**
: Print the version and exit.

# COMMANDS

**status** [**--json**]
: Show the daemon, its host control directory, per-job limits, shells, and
  Kit workers.

**jobs** [**--all**] [**--tree**] [**--json**]
: List Kit jobs, running first, then newest. **--tree** draws children under
  their parent. In a shell, only that session's jobs; from a host terminal,
  running trees, trees from the last hour, and the five newest. **--all** lists
  every recorded job. **--json** prints the full, unscoped document.

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
  patch. With **--** *CMD*, run *CMD* on that output with **SPLIT_ID**,
  **SPLIT_DIR**, **SPLIT_MANIFEST** and **SPLIT_OBJECTS** set; the split is
  removed if *CMD* exits 0 and kept otherwise.

**splits** [**--json**] [*ID*]
: List splits. **splits cancel** *ID* cancels a running split;
  **splits rm** *ID* removes a kept one.

**fanout** [**-n**] [**-b** *LABEL*=*STRING*]... [**:::** *LABEL* *CMD* [*ARG*...]]... | **collect** [**--json**] [**--timing**] [**--stderr**]
: Run branches at once on the same files (no copies). Print each branch's
  output in declaration order: its header, its stdout, then a failed branch's
  stderr. **--stderr** prints every branch's stderr. **collect** exits with
  the first nonzero branch status. Limits: 16 branches, 64 MiB of input, 16 MiB
  of combined branch output.

**config shell** [**bash**|**zsh**|**marsh**]
: Set, or with no name print, the default shell for this **MARSH_HOME**.

**kit install** *NAME* **--from** *REPOSITORY*@sha256:*DIGEST*
: Register a published Kit under *NAME* in this scope.

**workers reset** *KIT*[,*KIT*...]|**all**
: Remove idle Kit VMs, including quarantined ones. The next use creates a new
  VM.

**reset** [**--json**]
: Remove every VM this scope owns. The daemon keeps running. Refuses while
  shells or jobs are active. The guest home and results are kept. Exits
  nonzero if any cleanup could not be verified.

**stop** [**--json**]
: Remove every VM this scope owns and stop its daemon. Refuses while shells or
  jobs are active. The guest home and results are kept. Exits nonzero if any
  cleanup could not be verified.

**recover-home** *SLOT* **--discard**
: Discard a retained **--ephemeral-home** slot (0 to 15).

**mcp install** **codex**|**claude**|**sbx**
: Register marsh as an MCP server for this project. **codex** and **claude**
  launch it on demand; **sbx** gives selected sandboxes a shared per-project
  host broker.

**mcp install-published**|**remove-published codex** *NAME*
: Register or remove a published pipeline as a tool for host Codex. Only
  **codex** is supported.

**mcp** **serve**|**export-serve**|**start**|**stop**|**load**|**unpublish** ...
: The rest of the MCP commands. See **marsh mcp --help**.

**acp install-published**|**remove-published codex** *NAME*
: Register or remove a published ACP control tool with host Codex. Only
  **codex** is supported.

# SHELL COMMANDS

Inside the shell, these commands are also available:

**ps --marsh** [**--verbose**|**--json**], **top --marsh** [**--verbose**] [**--once**]
: Show this project's shell, Kit jobs, and ACP sessions. **top** cannot show
  CPU or memory.

**acp reserve** *AGENT*, **acp run** [**--reservation** *ID*] *AGENT* **&**, **acp list** [**--mine**] [**--wait** [*ID*]] [**--json**], **acp ask** *ID* *TEXT*, **acp prompt** [**--key** *UUID*] *ID* *TEXT*, **acp status** *ID* [*CURSOR*] [**--json**], **acp cancel** *ID*, **acp permissions** *ID*, **acp respond** *ID* *REQUEST* *OPTION*, **acp attach**|**release** *ID*, **acp stop** *ID*, **acp publish** *ID* **--name** *NAME* [**--kit** *KIT*|**--sandbox** *SANDBOX*], **acp unpublish** *NAME*
: Start and drive ACP agent sessions. Failures print **acp:** *MESSAGE* and
  exit 125; usage errors exit 2. See https://runmar.sh/docs/acp.html.

**mcp publish** *NAME* [**--description** *TEXT*] [**--kit** *KIT*|**--sandbox** *SANDBOX*] **--** '*PIPELINE*', **mcp load** *NAME* **--kit** *KIT*|**--sandbox** *SANDBOX*, **mcp unpublish** *NAME*
: Publish a fixed pipeline as an MCP tool. With **--kit** or **--sandbox** it is
  loaded there now. With neither, every agent Kit VM created from now on loads
  it before its first job. Kit VMs already running are not changed; use
  **mcp load** *NAME* **--kit** *KIT* to load into one now.

**split {** *label*: *commands* ... **} | join** [| *CMD*], **fanout {** ... **} | collect**
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

# FILES

*$MARSH_HOME/home* (default *~/.marsh/home*)
: Backing store for the guest home.

*$MARSH_HOME/config.json* (default *~/.marsh/config.json*)
: The default shell set by **marsh config shell**.

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
: A job start was refused because **MARSH_SPAWN** excludes the name.

130
: The job was interrupted.

# EXAMPLES

Start a shell in a project:

    cd ~/src/project && marsh

Ask two agents for independent fixes in private copies and look at both:

    marsh split -n ::: a claude -p 'fix the flaky test' \
                   ::: b pi -p 'fix the flaky test' | marsh join

**join** prints, for each branch, a header, its output, and a line giving the
files changed and the path of its **diff.patch**. Apply one with **git apply**.
Sample output: https://runmar.sh/docs/split.html.

Talk to one agent across turns, inside the shell:

    id=$(acp reserve pi-session)
    acp run --reservation "$id" pi-session &
    acp list --mine --wait "$id"
    acp ask "$id" 'What does src/main.rs do?'
    acp stop "$id"

Run tests and lint at once:

    marsh fanout -n -b test='make test' -b lint='make lint' | marsh collect

Publish a pipeline in the shell, then give it to Codex on the Mac:

    mcp publish lint -- 'make lint 2>&1'
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
