#!/bin/sh
set -eu

root=$(mktemp -d "${TMPDIR:-/tmp}/marsh-kit-cli.XXXXXX")
trap 'rm -rf "$root"' EXIT HUP INT TERM
bin="$root/bin"
mkdir -p "$bin"

make_fake() {
  name=$1
  cat >"$bin/$name" <<'EOF'
#!/bin/sh
{
  printf 'tty=%s\n' "$([ -t 0 ] && printf yes || printf no)"
  for argument in "$@"; do
    printf 'arg=%s\n' "$argument"
  done
} >"$MARSH_TEST_CAPTURE/invocation"
if [ ! -t 0 ]; then
  cat >"$MARSH_TEST_CAPTURE/stdin"
fi
exit "${MARSH_TEST_EXIT_STATUS:-0}"
EOF
  chmod 755 "$bin/$name"
}

make_fake claude
make_fake codex
make_fake pi

run_adapter() {
  kit=$1
  capture=$2
  shift 2
  home="$root/$kit-home"
  mkdir -p "$home" "$capture"
  PATH="$bin:$PATH" \
    MARSH_TEST_CAPTURE="$capture" \
    MARSH_SELECTED_HOME="$home" \
    sh "kits/marsh-$kit/marsh-entrypoint.sh" "$@"
}

assert_invocation() {
  capture=$1
  expected=$2
  printf '%s\n' "$expected" >"$root/expected"
  cmp "$root/expected" "$capture/invocation"
}

# No-argument pipes preserve stdin for every CLI. Codex alone needs its
# explicit noninteractive stdin form; Claude and Pi consume the pipe natively.
printf 'prompt over stdin' | run_adapter codex "$root/codex-pipe"
assert_invocation "$root/codex-pipe" 'tty=no
arg=--dangerously-bypass-approvals-and-sandbox
arg=exec
arg=-'
test "$(cat "$root/codex-pipe/stdin")" = 'prompt over stdin'

printf 'hi don, how are you?' | run_adapter codex "$root/codex-prompt-pipe" 'invoke the MCP tool on this input'
assert_invocation "$root/codex-prompt-pipe" 'tty=no
arg=--dangerously-bypass-approvals-and-sandbox
arg=exec
arg=-'
test "$(cat "$root/codex-prompt-pipe/stdin")" = 'invoke the MCP tool on this input

hi don, how are you?'

printf 'hi' | run_adapter codex "$root/codex-one-word-pipe" summarize
assert_invocation "$root/codex-one-word-pipe" 'tty=no
arg=--dangerously-bypass-approvals-and-sandbox
arg=exec
arg=-'
test "$(cat "$root/codex-one-word-pipe/stdin")" = 'summarize

hi'

printf 'hi' | run_adapter codex "$root/codex-explicit-prompt-pipe" --prompt review
assert_invocation "$root/codex-explicit-prompt-pipe" 'tty=no
arg=--dangerously-bypass-approvals-and-sandbox
arg=exec
arg=-'
test "$(cat "$root/codex-explicit-prompt-pipe/stdin")" = 'review

hi'

if printf 'input' | MARSH_TEST_EXIT_STATUS=23 run_adapter codex "$root/codex-prompt-failure" 'an instruction'; then
  echo 'Codex prompt adapter lost the CLI exit status' >&2
  exit 1
else
  test "$?" -eq 23
fi

printf 'prompt over stdin' | run_adapter claude "$root/claude-pipe"
assert_invocation "$root/claude-pipe" 'tty=no
arg=--dangerously-skip-permissions
arg=--settings
arg={"sandbox":{"enabled":false}}'
test "$(cat "$root/claude-pipe/stdin")" = 'prompt over stdin'

printf 'prompt over stdin' | run_adapter pi "$root/pi-pipe"
assert_invocation "$root/pi-pipe" 'tty=no
arg=--approve
arg=-e
arg=/opt/marsh/pi-gateway/mcp-gateway.mjs'
test "$(cat "$root/pi-pipe/stdin")" = 'prompt over stdin'

# Explicit subcommands and options reach the real CLI exactly once and in
# caller order after the Kit's fixed safety-mode option.
run_adapter codex "$root/codex-help" --help </dev/null
assert_invocation "$root/codex-help" 'tty=no
arg=--dangerously-bypass-approvals-and-sandbox
arg=--help'

run_adapter codex "$root/codex-login" login </dev/null
assert_invocation "$root/codex-login" 'tty=no
arg=--dangerously-bypass-approvals-and-sandbox
arg=login'

run_adapter codex "$root/codex-argv" exec --json 'two words' </dev/null
assert_invocation "$root/codex-argv" 'tty=no
arg=--dangerously-bypass-approvals-and-sandbox
arg=exec
arg=--json
arg=two words'

run_adapter claude "$root/claude-help" --help </dev/null
assert_invocation "$root/claude-help" 'tty=no
arg=--dangerously-skip-permissions
arg=--settings
arg={"sandbox":{"enabled":false}}
arg=--help'

run_adapter pi "$root/pi-help" --help </dev/null
assert_invocation "$root/pi-help" 'tty=no
arg=--approve
arg=-e
arg=/opt/marsh/pi-gateway/mcp-gateway.mjs
arg=--help'

# A bare Codex invocation on a terminal remains the interactive TUI. Python is
# used only to allocate the pseudoterminal; the adapter and fake executable are
# the same shell-level path exercised above.
PATH="$bin:$PATH" \
  MARSH_TEST_CAPTURE="$root/codex-tty" \
  MARSH_SELECTED_HOME="$root/codex-home" \
  python3 - "$(pwd)/kits/marsh-codex/marsh-entrypoint.sh" <<'PY'
import os
import pty
import subprocess
import sys

os.makedirs(os.environ["MARSH_TEST_CAPTURE"], exist_ok=True)
master, slave = pty.openpty()
try:
    process = subprocess.Popen(
        ["/bin/sh", sys.argv[1]],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=os.environ,
    )
finally:
    os.close(slave)
try:
    status = process.wait(timeout=10)
finally:
    os.close(master)
if status != 0:
    raise SystemExit(status)
PY
assert_invocation "$root/codex-tty" 'tty=yes
arg=--dangerously-bypass-approvals-and-sandbox'

# ACP mode's Claude Code executable (kits/marsh-claude/claude-cli.sh) keeps
# the SDK's arguments and disables Claude's Bash sandbox: it adds the setting
# when absent and merges it into an SDK-provided --settings object or file.
case "$(uname -m)" in aarch64|arm64) sdk_arch=arm64 ;; *) sdk_arch=x64 ;; esac
sdk="$root/sdk"
mkdir -p "$sdk/claude-agent-sdk-linux-$sdk_arch"
cp "$bin/claude" "$sdk/claude-agent-sdk-linux-$sdk_arch/claude"
run_claude_cli() {
  capture=$1
  shift
  mkdir -p "$capture"
  MARSH_TEST_CAPTURE="$capture" MARSH_TEST_CLAUDE_SDK="$sdk" \
    sh kits/marsh-claude/claude-cli.sh "$@" </dev/null
}
run_claude_cli "$root/acp-plain" --output-format stream-json --verbose
assert_invocation "$root/acp-plain" 'tty=no
arg=--output-format
arg=stream-json
arg=--verbose
arg=--settings
arg={"sandbox":{"enabled":false}}'

run_claude_cli "$root/acp-merge" --settings '{"model":"x","sandbox":{"enabled":true,"x":1}}' -p hi
assert_invocation "$root/acp-merge" 'tty=no
arg=--settings
arg={"model":"x","sandbox":{"enabled":false,"x":1}}
arg=-p
arg=hi'

printf '%s' '{"permissions":{"allow":[]}}' >"$root/sdk-settings.json"
run_claude_cli "$root/acp-file" "--settings=$root/sdk-settings.json"
assert_invocation "$root/acp-file" 'tty=no
arg=--settings={"permissions":{"allow":[]},"sandbox":{"enabled":false}}'

run_claude_cli "$root/acp-subcommand" auth status --json
assert_invocation "$root/acp-subcommand" 'tty=no
arg=auth
arg=status
arg=--json'

# Every agent Kit selects ACP mode only from a leading --acp, which
# packaging/agents.json appends; the CLI path never sees it.
for kit in claude codex pi; do
  grep -q '^if \[ "${1:-}" = --acp \]; then$' "kits/marsh-$kit/marsh-entrypoint.sh"
done
grep -q '^INITIAL_AGENT_MODE=${INITIAL_AGENT_MODE:-agent-full-access}$' kits/marsh-codex/acp-entrypoint.sh
