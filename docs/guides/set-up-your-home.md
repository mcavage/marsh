# Set up your marsh home

From your Mac, put marsh's dotfiles and agent settings in `~/.marsh/home`.
Inside marsh, that directory appears as your home (`~`). Its files survive
`marsh stop` and VM resets.

## Agent settings and skills

Use these paths on your Mac:

| Agent | Skills | Global instructions |
|---|---|---|
| Pi | `~/.marsh/home/.agents/skills/NAME/SKILL.md` | `~/.marsh/home/.pi/agent/AGENTS.md` |
| Codex | `~/.marsh/home/.agents/skills/NAME/SKILL.md` | `~/.marsh/home/.codex/AGENTS.md` |
| Claude | `~/.marsh/home/.claude/skills/NAME/SKILL.md` | `~/.marsh/home/.claude/CLAUDE.md` |

Put shell startup files here too: `~/.marsh/home/.bashrc` on the Mac is
`~/.bashrc` inside marsh. See [Choosing your shell](../shells.md) for startup
files and [The shell](../shell.md#editor-settings) for editor settings.

Jobs share this home, including any saved logins and history. If you keep
dotfiles in Git, track the files you choose rather than the entire home.

For example, add an alias and prompt in the **marsh shell**:

<!-- doc-test: shell stdout="ll=" -->
```sh
cat >> ~/.bashrc <<'EOF'
alias ll='ls -lah'
PS1='\w \$ '
EOF
source ~/.bashrc
alias ll
```

## Git identity

The first time marsh starts a session, and again with `marsh init`, it creates
`~/.marsh/home` and, if that home has no `.gitconfig`, copies your git name and
email from your Mac's `git config --global` into one. Every shell and Kit job
shares that home, so commits made in them have an author. Only those two values
are copied: not your credential helper, signing key, or other settings. An
existing `.gitconfig` is never changed, so edit `~/.marsh/home/.gitconfig` (or
run `git config --global ...` inside marsh) to change the identity. If your Mac
has no identity, `marsh init` says so; set one there and run it again.

## Connect an MCP server

Save connections for one registered command in `~/.marsh/sbxenvs/NAME.yaml`.
Create the `sbxenvs` directory if needed. For example, save this as
`~/.marsh/sbxenvs/pi.yaml` on your Mac:

```yaml
schemaVersion: "1"
mcp:
  servers:
    - name: granola
      url: https://mcp.granola.ai/mcp
```

Exit your marsh shells, run `marsh stop` from your Mac terminal, then start
marsh again. The next Pi job loads the connection. If it reports that sign-in
is required, run this on your Mac, then retry:

```sh
sbx mcp auth granola
```

To load a connection for every Kit and the shell, put it in
`~/.marsh/sbxenv.yaml` instead. Shared entries are added to each command's
entries; do not repeat the same server in both files.

Loaded servers are shared by jobs in that Kit VM, across projects. Removing
an entry stops future automatic loading; it does not remove the server's
Docker Sandboxes registration or sign-in. Manage those with `sbx mcp` on the Mac.

### Local servers

Use `command` and `args` instead of `url`:

```yaml
schemaVersion: "1"
mcp:
  servers:
    - name: my-reader
      command: /absolute/path/to/my-reader
      args: ["--readonly"]
```

**Local MCP servers run on your Mac with your account's permissions.**
Keep their executable, scripts, and configuration outside directories that
jobs can modify. Use the server's host-side authentication setup; keep keys
out of YAML and command arguments.

## Mount a reference directory

Add `additionalWorkspaces` to the same YAML file. For example, this makes an
existing Mac directory available to Pi at `/Users/me/reference`, read-only:

```yaml
schemaVersion: "1"
additionalWorkspaces:
  - path: /Users/me/reference
    readOnly: true
```

Replace the path with your directory's absolute path. YAML paths do not expand
`~` or environment variables. Set `readOnly: true` for files the agent should
only read; the default is read-write. You can include both `mcp` and
`additionalWorkspaces` in one file.

Restart marsh after saving, as above. Child jobs inherit their parent's extra
directories. Agents in a split do not receive extra directories.

With a different `MARSH_HOME`, use its `home`, `sbxenv.yaml`, and `sbxenvs`
paths instead. Ephemeral-home sessions skip these additions.

See [Configuration](../configuration.md#servers-and-extra-directories) for
file rules and limits, [Troubleshooting](../troubleshooting.md#environment-files-and-mcp-connections)
for setup errors, and [Security](../security.md#mcp-connections) for MCP access.
