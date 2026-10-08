#!/usr/bin/env python3
"""Real public-shell byte journeys. No text decoding of shell output.

Each row records stdout/stderr hex, raw source and argv, status and file effects.
Nothing in this harness launches Bash on behalf of a candidate implementation.
The GNU executable is supplied explicitly as a separately measured oracle.

Usage: python3 tests/acceptance/bash_lossless_bytes.py --candidate PATH \
    --oracle GNU_BASH_5_3_20 --evidence IGNORED_DIRECTORY [--pristine PATH]

This local public-entry gate does not replace packaged stock-SBX/Cloud UAT.
Each role uses the same owned native fixture path, with no output normalization.
"""
import argparse, hashlib, json, os, pathlib, shutil, signal, subprocess, tempfile, time

BAD = b'\xff\xfe'
MIXED = b'caf\xc3\xa9\xff\xfe\xef\xbf\xbd\xee\x80\x80'
# ASCII source produces raw bytes without relying on raw-source parsing.
INIT = b"v=$(printf '\\377\\376'); "
MIX = b"v=$(printf 'caf\\303\\251\\377\\376\\357\\277\\275\\356\\200\\200'); "
DUMP = b"/usr/bin/python3 dump.py"
CASES = []

def case(name, source, *, args=(), env=None, stdin=b'', files=None, mode='command'):
    CASES.append(dict(name=name, source=source, args=args, env=env or {}, stdin=stdin,
                      files=files or {}, mode=mode))

case('ansi-c-literal-bytes', b"v=$'\\377\\376'; printf '%s' \"$v\"")
case('ansi-c-literal-nul', b"v=$'a\\0b'; printf '%s' \"$v\"")
case('substitution-scalar-printf', INIT+b"printf '%s' \"$v\"")
case('substitution-mixed-distinct', MIX+b"printf '%s' \"$v\"")
case('substitution-all-bytes', b'v=$(/usr/bin/cat payload); printf "%s" "$v"', files={b'payload':bytes(range(256))})
case('all-bytes-export-native-child', b'v=$(/usr/bin/cat payload); export BYTE_VALUE="$v"; ( '+DUMP+b' "$v" )', files={b'payload':bytes(range(256))})
case('substitution-native-child', INIT+b"( printf '%s' \"$v\" )")
case('substitution-pipeline', INIT+b"printf '%s' \"$v\" | /usr/bin/od -An -tx1")
case('substitution-status-lf', b"v=$(printf '\\377\\376\\n\\n'; exit 23); r=$?; printf '%s:%s' \"$r\" \"$v\"")
case('substitution-nul-warning', b"v=$(printf 'a\\0\\377\\n\\n'; exit 23); r=$?; printf '%s:%s' \"$r\" \"$v\"")
case('quoted-external-argv', INIT+DUMP+b' "$v"')
case('unquoted-external-argv', b"v=$(printf 'a\\377 b\\376'); "+DUMP+b' $v')
case('array-values', INIT+b'a=("$v" x "$v"); '+DUMP+b' "${a[@]}"')
case('sparse-array-append', INIT+b'a=([4]="$v"); a[4]+="$v"; '+DUMP+b' "${a[@]}"')
case('associative-key-value', INIT+b'declare -A a; a["$v"]="$v"; '+DUMP+b' "${!a[@]}" "${a[$v]}"')
case('associative-native-snapshot', INIT+b'declare -A a; a["$v"]="$v"; ( '+DUMP+b' "${!a[@]}" "${a[$v]}" )')
case('positional-star-at', INIT+b'set -- "$v" "b$v"; '+DUMP+b' "$@" "$*"')
case('function-arguments', INIT+b'f() { '+DUMP+b' "$@"; }; f "$v"')
case('default-alternate-assign', INIT+b'unset x; '+DUMP+b' "${x:-$v}" "${x:=$v}" "${x:+$v}" "$x"')
case('export-native-env', INIT+b'export BYTE_VALUE="$v"; '+DUMP)
case('prefix-native-env', INIT+b'BYTE_VALUE="$v" '+DUMP)
case('export-subshell-env', INIT+b'export BYTE_VALUE="$v"; ( '+DUMP+b' )')
case('host-nonutf8-env', b'printf "%s" "$BYTE_VALUE"', env={b'BYTE_VALUE': MIXED})
case('host-nonutf8-argv', b'printf "%s" "$1"', args=(MIXED,))
case('echo-data', INIT+b'echo -n "$v"')
case('echo-escape', b"echo -ne '\\0377\\0376'")
case('printf-b', INIT+b"printf '%b' \"$v\"")
case('printf-format-bytes', INIT+b'printf "$v:%s" ok')
case('printf-v', b"printf -v v '\\377\\376'; r=$?; printf '%s:%s' \"$r\" \"$v\"")
case('printf-v-nul', b"printf -v v 'a\\0b'; r=$?; printf '%s:%s' \"$r\" \"$v\"")
case('printf-v-assoc', b"declare -A a; k=$(printf '\\377'); printf -v 'a[$k]' '\\376'; "+DUMP+b' "${!a[@]}" "${a[$k]}"')
case('printf-q-roundtrip', INIT+b"q=$(printf '%q' \"$v\"); eval \"set -- $q\"; "+DUMP+b' "$@"')
case('declare-print-roundtrip', INIT+b'declare -a a=("$v"); q=$(declare -p a); unset a; eval "$q"; '+DUMP+b' "${a[@]}"')
case('read-r', b'IFS= read -r v; r=$?; printf "%s:%s" "$r" "$v"', stdin=MIXED+b'\\tail\n')
case('read-array', b'read -r -a a; '+DUMP+b' "${a[@]}"', stdin=b'\xff a\xfe\n')
case('read-byte-count', b'IFS= read -r -N 2 v; printf "%s" "$v"', stdin=BAD+b'\n')
case('mapfile', b'mapfile -t a; '+DUMP+b' "${a[@]}"', stdin=BAD+b'\n'+MIXED+b'\n')
case('mapfile-nul', b'mapfile -t a; '+DUMP+b' "${a[@]}"', stdin=b'a\0b\n\xff\0c\n')
case('read-nul', b'IFS= read -r v; printf "%s" "$v"', stdin=b'a\0b\xff\n')
case('read-controls', b'IFS= read -r v; printf "%s" "$v"', stdin=b'a\x01\x02\x03\x04\x05\x1b\x7f\xff\n')
case('path-redirect-read', INIT+b'printf data > "f$v"; /usr/bin/cat "f$v"')
case('path-glob', DUMP+b' f*', files={b'f\xff':b'A', b'f\xfe':b'B', b'f\xef\xbf\xbd':b'C', b'f\xee\x80\x80':b'D'})
case('path-glob-byte-pattern', INIT+DUMP+b' f${v:0:1}*', files={b'f\xffx':b'A', b'f\xfe':b'B'})
case('path-command-name', b'./cmd*', files={b'cmd\xff':b'#!/usr/bin/python3\nimport os; os.write(1, b"EXEC_OK")\n'})
case('path-cd', b'cd dir*; printf x > result; /usr/bin/pwd', files={b'dir\xff/seed': b'yes'})
case('raw-script-literal', b'v="'+MIXED+b'"; printf "%s" "$v"', mode='file')
case('raw-stdin-literal', b'v="'+MIXED+b'"; printf "%s" "$v"\n', mode='stdin')
case('raw-command-literal', b'v="'+MIXED+b'"; printf "%s" "$v"')
case('raw-eval-literal', INIT+b'eval \'printf "%s" "\'"$v"\'"\'')
case('raw-source-literal', b'. ./raw-source', files={b'raw-source':b'v="'+MIXED+b'"; printf "%s" "$v"\n'})
case('ifs-literal-not-split', b"IFS=:; v='a:b'; "+DUMP+b' x:y $v "a:b"')
case('ifs-nonwhitespace-empty-fields', b"IFS=:; v=':a::b:'; "+DUMP+b' $v')
case('ifs-literal-adjacent-expanded-delimiter', b"IFS=:; v=':'; "+DUMP+b' a${v}${v}b a${v} ${v}b')
case('ifs-whitespace-nonwhitespace', b"IFS=' :'; v='  : a  :  : b  :  '; "+DUMP+b' $v')
case('ifs-empty-star', b"IFS=; set -- a b; "+DUMP+b' "$*" "$@"')
case('history-raw-load', b'history -c; history -r ./hist; history', files={b'hist': b'#comment\nprintf \'\xff\'\n#1234567890\nprintf \'\xfe\'\n'})
case('history-raw-save', INIT+b'history -c; history -s "$v"; history -w ./saved; /usr/bin/cat ./saved')
case('sparse-negative-index', b'a=([4]=old [9]=last); a[-1]+=x; printf "%s:%s" "${a[9]}" "${a[-1]}"')
case('brace-literal-ifs', b'IFS=:; '+DUMP+b' {a:b,c:d}')
case('brace-empty-fields', DUMP+b' {,} ""')
case('brace-scalar-assignment', b'v={a,b}; printf "%s" "$v"')
case('inherited-invalid-env-unset', b"unset $'BAD_\\377'; /usr/bin/python3 -c 'import os; os.write(1, os.environb.get(b\"BAD_\\xff\", b\"missing\"))'", env={b'BAD_\xff': BAD})
case('inherited-invalid-env-name', b"/usr/bin/python3 -c 'import os; os.write(1, os.environb.get(b\"BAD_\\xff\", b\"missing\"))'", env={b'BAD_\xff': BAD})
for locale in (b'C', b'C.utf8'):
    tag=locale.decode('ascii')
    case('read-count-conservation-'+tag, b'IFS= read -r -N 1 v; printf "<%s>" "$v"; /usr/bin/cat', stdin=b'\xc3X\xfeY', env={b'LC_ALL':locale})
    case('read-delimiter-first-byte-'+tag, b"IFS= read -r -d $'\\303\\251' v; printf '<%s>' \"$v\"; /usr/bin/cat", stdin=b'a\xc3\xa9b', env={b'LC_ALL':locale})
    case('length-slice-'+tag, MIX+b'printf "%s:" "${#v}"; '+DUMP+b' "${v:0:4}" "${v:4:1}" "${v: -2}"', env={b'LC_ALL':locale})
    case('ifs-invalid-'+tag, b"IFS=$(printf '\\377'); v=$(printf 'a\\377b\\376c'); "+DUMP+b' $v', env={b'LC_ALL':locale})
    case('ifs-multibyte-'+tag, b"IFS=$(printf '\\303\\251'); v=$(printf 'a\\303\\251b\\251c'); "+DUMP+b' $v', env={b'LC_ALL':locale})
    case('substring-pattern-'+tag, INIT+b'printf "%s" "${v#?}"', env={b'LC_ALL':locale})
    case('replace-pattern-'+tag, INIT+b'printf "%s" "${v/\xff/X}"', env={b'LC_ALL':locale}, mode='file')

HELPER=b'''import json, os, sys
print(json.dumps({"argv":[os.fsencode(a).hex() for a in sys.argv[1:]], "env":os.environb.get(b"BYTE_VALUE", b"").hex()}, sort_keys=True))
'''

def digest(path): return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()

def effects(root):
    result=[]
    # os.walk bytes preserves non-UTF8 names and symlink targets.
    for directory, dirs, files in os.walk(os.fsencode(root)):
        for name in sorted(dirs+files):
            path=os.path.join(directory,name); rel=os.path.relpath(path,os.fsencode(root))
            if rel == b'home' or rel.startswith(b'home/'): continue
            if os.path.islink(path): item=dict(kind='symlink', target_hex=os.readlink(path).hex())
            elif os.path.isdir(path): item=dict(kind='directory')
            else:
                data=pathlib.Path(os.fsdecode(path)).read_bytes()
                item=dict(kind='file', data_hex=data.hex(), executable=bool(os.stat(path).st_mode & 0o111))
            result.append(dict(path_hex=rel.hex(), **item))
    return sorted(result,key=lambda row:row['path_hex'])

def execute(binary, test, root):
    if root.exists(): shutil.rmtree(root) # only this harness's owned case directory
    root.mkdir(parents=True); (root/'home').mkdir(); (root/'dump.py').write_bytes(HELPER)
    for name, data in test['files'].items():
        path=root/os.fsdecode(name); path.parent.mkdir(parents=True,exist_ok=True); path.write_bytes(data)
        if name.startswith(b'cmd'): path.chmod(0o700)
    stdin=test['stdin']
    base=[os.fsencode(binary), b'--noprofile', b'--norc']
    if test['mode']=='file':
        (root/'script.sh').write_bytes(test['source']); argv=base+[b'./script.sh']+list(test['args'])
    elif test['mode']=='stdin': argv=base; stdin=test['source']
    else: argv=base+[b'-c',test['source'],b'byte-case']+list(test['args'])
    history=root/'home'/'history'
    fd=os.open(history,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.close(fd)
    env={b'PATH':b'/usr/bin:/bin', b'HISTFILE':os.fsencode(history), b'HOME':os.fsencode(root/'home'), b'LC_ALL':b'C', b'USER':b'node', b'LOGNAME':b'node', b'PYTHONUTF8':b'0'}
    env.update(test['env'])
    start=time.monotonic(); timed_out=False
    process=subprocess.Popen(argv,cwd=root,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    try: stdout,stderr=process.communicate(stdin,timeout=10)
    except subprocess.TimeoutExpired:
        timed_out=True; os.killpg(process.pid,signal.SIGKILL); stdout,stderr=process.communicate()
    return dict(name=test['name'], source_hex=test['source'].hex(), mode=test['mode'], args_hex=[a.hex() for a in test['args']],
                supplied_env_hex={k.hex():v.hex() for k,v in test['env'].items()}, stdin_hex=stdin.hex(),
                stdout_hex=stdout.hex(),stderr_hex=stderr.hex(),status=process.returncode,timed_out=timed_out,
                effects=effects(root),elapsed=time.monotonic()-start,pid=process.pid,pgid=process.pid)

def bounded_probe(argv):
    process=subprocess.Popen(argv,env={'PATH':'/usr/bin:/bin','LC_ALL':'C'},stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    try: stdout,stderr=process.communicate(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid,signal.SIGKILL); process.communicate()
        raise RuntimeError('owned prerequisite probe timed out')
    if process.returncode: raise RuntimeError('prerequisite probe failed: '+repr(stderr))
    return stdout


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate',required=True,type=pathlib.Path)
    parser.add_argument('--oracle',required=True,type=pathlib.Path)
    parser.add_argument('--pristine',type=pathlib.Path)
    parser.add_argument('--evidence',required=True,type=pathlib.Path)
    parser.add_argument('--source-manifest',type=pathlib.Path)
    parser.add_argument('--utf8-locale',help='Available UTF8 locale; otherwise discovered with locale -a')
    parser.add_argument('--only',action='append')
    args=parser.parse_args(); args.evidence.mkdir(parents=True,exist_ok=True)
    binaries={'oracle':args.oracle.resolve(),'candidate':args.candidate.resolve()}
    if args.pristine: binaries['pristine']=args.pristine.resolve()
    identities={role:dict(path=str(path),sha256=digest(path)) for role,path in binaries.items()}
    if identities['oracle']['sha256']==identities['candidate']['sha256']:
        parser.error('candidate and oracle must be distinct artifacts, not a self-comparison')
    version=bounded_probe([str(binaries['oracle']),'--version'])
    if b'version 5.3.20' not in version.splitlines()[0]: parser.error('GNU Bash 5.3.20 oracle required')
    locales=bounded_probe(['/usr/bin/locale','-a']).splitlines()
    if args.utf8_locale:
        utf8=os.fsencode(args.utf8_locale)
        if utf8 not in locales: parser.error('requested UTF8 locale is not installed')
    else:
        utf8=next((name for name in locales if b'utf8' in name.lower().replace(b'-',b'')),None)
        if utf8 is None: parser.error('an installed UTF8 locale is required')
    if args.only and set(args.only)-{test['name'] for test in CASES}: parser.error('unknown case selector')
    script=pathlib.Path(__file__).read_bytes()
    (args.evidence/'harness.py').write_bytes(script)
    report=dict(artifacts=identities,oracle_version_hex=version.hex(),utf8_locale_hex=utf8.hex(),
                harness_sha256=hashlib.sha256(script).hexdigest(),cases=[],qualification='local public-shell bytes only; not stock/Cloud/release approval')
    if args.source_manifest:
        manifest=args.source_manifest.read_bytes(); (args.evidence/'source-manifest.json').write_bytes(manifest)
        report['source_manifest_sha256']=hashlib.sha256(manifest).hexdigest()
    checks=('stdout_hex','stderr_hex','status','effects')
    with tempfile.TemporaryDirectory(prefix='marsh-byte-e2e-') as directory:
        work=pathlib.Path(directory)
        with (args.evidence/'observations.jsonl').open('w') as output:
            for original in CASES:
                if args.only and original['name'] not in args.only: continue
                test=dict(original,env=dict(original['env']))
                if test['env'].get(b'LC_ALL')==b'C.utf8': test['env'][b'LC_ALL']=utf8
                rows={}
                for role,binary in binaries.items():
                    row=execute(binary,test,work/test['name']); rows[role]=row
                    output.write(json.dumps(dict(role=role,**row),sort_keys=True)+'\n'); output.flush()
                expected=rows['oracle']; actual=rows['candidate']
                delta=[field for field in checks if expected[field]!=actual[field]]
                exact_oracle={
                    'ansi-c-literal-bytes':BAD,
                    'ansi-c-literal-nul':b'a',
                    'substitution-scalar-printf':BAD,
                    'substitution-mixed-distinct':MIXED,
                    'substitution-all-bytes':bytes(range(1,256)),
                    'host-nonutf8-env':MIXED,
                    'host-nonutf8-argv':MIXED,
                    'echo-escape':BAD,
                    'raw-script-literal':MIXED,
                    'raw-stdin-literal':MIXED,
                    'raw-command-literal':MIXED,
                    'raw-eval-literal':BAD,
                    'raw-source-literal':MIXED,
                }
                if test['name'] in exact_oracle and (expected['status']!=0 or expected['stdout_hex']!=exact_oracle[test['name']].hex()):
                    delta.append('oracle-fixture-contract')
                if expected['timed_out'] or actual['timed_out']: delta.append('timeout')
                report['cases'].append(dict(name=test['name'],passed=not delta,differences=delta))
    for role,path in binaries.items():
        identities[role]['stable']=identities[role]['sha256']==digest(path)
    report['passed']=all(row['passed'] for row in report['cases']) and all(info['stable'] for info in identities.values())
    (args.evidence/'result.json').write_text(json.dumps(report,indent=2))
    print(json.dumps(dict(passed=report['passed'],cases=len(report['cases']),failed=sum(not row['passed'] for row in report['cases']))))
    raise SystemExit(0 if report['passed'] else 1)

if __name__=='__main__': main()
