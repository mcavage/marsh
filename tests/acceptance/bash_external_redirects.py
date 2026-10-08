#!/usr/bin/env python3
"""Real GNU/candidate callers for external redirection process boundaries."""
import argparse
import hashlib
import json
import os
import pathlib
import signal
import subprocess
import tempfile
import time

CASES = [
    ("args-parent", b"v=before; /bin/echo \"${ v=after; printf hi; }\"; printf 'state:%s' \"$v\""),
    ("prefix-parent", b"v=before; W=${ v=after; printf val; } /usr/bin/printenv W; printf 'state:%s' \"$v\""),
    ("prefix-before-literal", b"v=before; W=${ v=after; printf val; } /bin/echo hi >out; printf 'state:%s' \"$v\""),
    ("prefix-before-dynamic", b"v=before; W=${ v=after; printf val; } /bin/echo hi >\"${ printf '%s' \"$v\"; }\"; printf 'state:%s' \"$v\""),
    ("prefix-not-visible", b"unset W; W=val /bin/echo hi >\"${W-none}\""),
    ("prefix-sequential", b"a=old; a=new b=$a /usr/bin/printenv b >out; printf '%s' \"$a\""),
    ("prefix-failed-literal", b"v=before; W=${ v=after; printf val; } /bin/echo hi >missing/out; printf 'state:%s status:%s' \"$v\" \"$?\""),
    ("builtin-prefix-redir", b"v=before; W=${ v=after; printf val; } printf hi >\"${ printf '%s' \"$v\"; }\"; printf 'state:%s' \"$v\""),
    ("builtin-prefix-hidden", b"unset W; W=val printf hi >\"${W-none}\""),
    ("special-prefix-hidden", b"set -o posix; unset W; W=val : >\"${W-none}\"; printf '%s' \"$W\""),
    ("output-state", b"v=before; /bin/echo hi >\"${ v=after; printf out; }\"; printf 'state:%s' \"$v\""),
    ("command-output-state", b"v=before; command /bin/echo hi >\"${ v=after; printf out; }\"; printf 'state:%s' \"$v\""),
    ("here-state", b"v=before; /bin/cat <<DOC\n${ v=after; printf hi; }\nDOC\nprintf 'state:%s' \"$v\""),
    ("here-reply", b"v=before; /bin/cat <<DOC\n${| v=after; REPLY=$'r\\n'; printf visible; }\nDOC\nprintf 'state:%s' \"$v\""),
    ("here-pid", b"parent=$BASHPID; /bin/cat <<DOC\n${ if [[ $BASHPID != $parent ]]; then printf CHILD; else printf PARENT; fi; }:$BASH_SUBSHELL\nDOC"),
    ("herestring-pid", b"parent=$BASHPID; /bin/cat <<<\"${ if [[ $BASHPID != $parent ]]; then printf CHILD; else printf PARENT; fi; }:$BASH_SUBSHELL\""),
    ("quoted-here", b"v=before; /bin/cat <<'DOC'\n${ v=after; printf hi; }\nDOC\nprintf 'state:%s' \"$v\""),
    ("variable-fd", b"unset fd; /bin/echo hi {fd}>out; printf 'fd:%s' \"${fd-unset}\""),
    ("variable-fd-dynamic", b"unset fd; v=before; /bin/echo hi {fd}>\"${ v=after; printf out; }\"; printf 'fd:%s state:%s' \"${fd-unset}\" \"$v\""),
    ("builtin-variable-fd", b"unset fd; printf hi {fd}>out; printf 'fd:%s' \"$fd\"; printf WRITE >&\"$fd\""),
    ("arithmetic-state", b"i=0; /bin/echo hi >\"$((++i))\"; printf 'i:%s' \"$i\""),
    ("parameter-state", b"unset v; /bin/echo hi >\"${v:=out}\"; printf 'v:%s' \"${v-unset}\""),
    ("redirect-order", b"v=before; /bin/echo hi >out 3>\"${ v=after; printf first; }\" 4>\"${ printf '%s' \"$v\"; }\"; printf 'v:%s' \"$v\""),
    ("process-substitution", b"v=before; /bin/cat < <(v=after; printf hi); printf 'state:%s job:%s' \"$v\" \"${!:-none}\""),
    ("exit-no-parent-trap", b"trap 'printf EXIT' EXIT; /bin/echo hi >\"${ exit 12; }\"; printf 'status:%s' \"$?\""),
    ("local-exit-trap", b"trap 'printf PARENT' EXIT; /bin/echo hi >\"${ trap 'printf CHILD' EXIT; exit 12; }\"; printf 'status:%s' \"$?\""),
    ("raw-here", b"/bin/cat <<DOC\n\xff\xfe ${ printf '\\377\\376'; }\nDOC"),
    ("raw-prefix", b"W=$'\\377\\376' /usr/bin/printenv W >out; printf DONE"),
    ("unknown-command", b"v=before; no_such_native_redirect_command >\"${ v=after; printf out; }\"; printf 'v:%s status:%s' \"$v\" \"$?\""),
    ("assignment-redir", b"v=before; x=hello >\"${ v=after; printf out; }\"; printf 'x:%s v:%s' \"$x\" \"$v\""),
    ("pipeline-redir", b"v=before; /bin/echo hi >\"${ v=after; printf out; }\" | /bin/cat; printf 'v:%s' \"$v\""),
    ("prefix-trace", b"set -x; v=before; W=${ v=after; printf val; } /bin/echo hi >out; printf '%s' \"$v\""),
]


def invoke(binary, source):
    with tempfile.TemporaryDirectory(prefix='marsh-redirect-') as directory:
        root = pathlib.Path(directory)
        (root / 'history').touch(mode=0o600)
        env = {'HOME': directory, 'HISTFILE': str(root / 'history'), 'PATH': '/usr/bin:/bin', 'LC_ALL': 'C'}
        start = time.monotonic()
        child = subprocess.Popen([str(binary), '--noprofile', '--norc', '-c', source, 'shell'], cwd=directory,
                                 env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, start_new_session=True)
        forced = False
        try:
            out, err = child.communicate(timeout=12)
        except subprocess.TimeoutExpired:
            forced = True
            os.killpg(child.pid, signal.SIGKILL)
            out, err = child.communicate(timeout=3)
        files = {p.name: p.read_bytes().hex() for p in root.iterdir() if p.is_file() and p.name != 'history'}
        return {'status': child.returncode, 'stdout_hex': out.hex(), 'stderr_hex': err.hex(),
                'files': files, 'forced': forced, 'elapsed': time.monotonic() - start}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--candidate', required=True)
    parser.add_argument('--gnu', required=True)
    parser.add_argument('--evidence', required=True)
    args = parser.parse_args()
    candidate, gnu = pathlib.Path(args.candidate).resolve(), pathlib.Path(args.gnu).resolve()
    rows = []
    for name, source in CASES:
        expected, actual = invoke(gnu, source), invoke(candidate, source)
        keys = ('status', 'stdout_hex', 'stderr_hex', 'files', 'forced')
        rows.append({'case': name, 'source_hex': source.hex(), 'gnu': expected, 'candidate': actual,
                     'pass': not expected['forced'] and not actual['forced'] and all(expected[k] == actual[k] for k in keys)})
    result = {'candidate': str(candidate), 'candidate_sha256': hashlib.sha256(candidate.read_bytes()).hexdigest(),
              'gnu': str(gnu), 'gnu_sha256': hashlib.sha256(gnu.read_bytes()).hexdigest(),
              'harness_sha256': hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(), 'rows': rows,
              'summary': {'total': len(rows), 'passed': sum(row['pass'] for row in rows),
                          'failed': [row['case'] for row in rows if not row['pass']]}}
    evidence = pathlib.Path(args.evidence)
    assert not evidence.exists(), 'preserve prior evidence'
    evidence.parent.mkdir(parents=True, exist_ok=True)
    evidence.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result['summary']))
    return int(any(not row['pass'] for row in rows))


if __name__ == '__main__':
    raise SystemExit(main())
