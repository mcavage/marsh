#!/usr/bin/env python3
"""Public shell entry byte builtin differential callers; no text normalization.

GNU 5.3.20 is an external oracle, never the candidate's implementation. Each
invocation owns a fresh SID/PGID. Retain stdout/stderr/status/files as exact hex.
Run with --candidate only after the atomic parser/core/builtin graph coheres.
"""
import argparse
import dataclasses
import hashlib
import json
import os
import fcntl
import pty
import select
import termios
from pathlib import Path
import signal
import subprocess
import tempfile
import time

ORACLE_SHA = "d1fb5699959b164bc05f834bb32f4fdda96dc0457e01eb8357943b7803e6f2c0"
ROOT = Path(__file__).resolve().parents[2]
NATIVE = b""
NATIVE_SOURCE = br'''#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static void put(const void *p, size_t n) { const char *b=p; while(n) { ssize_t k=write(1,b,n); if(k<=0) _exit(90); b+=k; n-=k; } }
static void frame(const char *s) { uint32_t n=s?(uint32_t)strlen(s):UINT32_MAX; unsigned char h[4]={n>>24,n>>16,n>>8,n}; put(h,4); if(s) put(s,n); }
int main(int argc, char **argv) { frame(argv[0]); frame(argc>1?argv[1]:NULL); frame(getenv("RAW")); frame(getenv("BAD_\377")); return 0; }
'''

def frame(value):
    return b"\xff\xff\xff\xff" if value is None else len(value).to_bytes(4,'big')+value


def build_native(evidence):
    output=evidence/'native-child'
    p=subprocess.Popen(['cc','-O0','-x','c','-o',str(output),'-'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    try: out,err=p.communicate(NATIVE_SOURCE,timeout=30)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid,signal.SIGKILL);out,err=p.communicate();raise
    (evidence/'native-child.c').write_bytes(NATIVE_SOURCE)
    (evidence/'native-build.json').write_text(json.dumps({'status':p.returncode,'pid':p.pid,'pgid':p.pid,'stdout_hex':out.hex(),'stderr_hex':err.hex(),'source_sha256':hashlib.sha256(NATIVE_SOURCE).hexdigest(),'binary_sha256':sha(output) if p.returncode==0 else None},indent=2)+'\n')
    if p.returncode: raise RuntimeError('native child fixture compilation failed')
    return output.read_bytes()

@dataclasses.dataclass
class Case:
    name: str
    source: bytes
    stdin: bytes = b""
    args: tuple = ()
    files: tuple = ()
    env: tuple = ()
    expected: object = None  # independently specified (stdout, stderr, status)
    hold_open: bool = False  # parent holds input open to exercise deadline/partial units


def cases():
    raw = b"\xff\xfe\xef\xbf\xbd\xee\x80\x80\xc3X"
    yield Case("echo-raw-argv", b'echo "$1"', args=(raw,), expected=(raw+b"\n", b"", 0))
    yield Case("echo-escape-order", br"echo -eE '\0377'; echo -Ee '\0377\0376\cIGNORED' lost", expected=(b"\\0377\n\xff\xfe", b"", 0))
    yield Case("echo-posix-xpg-options", br"shopt -s xpg_echo; set -o posix; echo -nE '\0377'",expected=(b"-nE \xff\n",b"",0))
    yield Case("echo-invalid-option", b'echo -neX "$1"', args=(raw,), expected=(b"-neX "+raw+b"\n", b"", 0))
    yield Case("echo-all-bytes", b'echo -n "$1"', args=(bytes(range(1,256)),), expected=(bytes(range(1,256)),b"",0))
    yield Case("printf-raw-format", b'printf "$1"', args=(b"before\xff\xfeafter",), expected=(b"before\xff\xfeafter",b"",0))
    yield Case("printf-raw-s-cycles", b"printf '<%s>' \"$1\" '' \"$2\"", args=(raw,b"\xfe"), expected=(b"<"+raw+b"><><\xfe>",b"",0))
    yield Case("printf-b-c-stop", br"printf '<%b>X' '\0377\0376\c' other", expected=(b"<\xff\xfe",b"",0))
    yield Case("printf-octal-modes", br"printf '\377|\0377|%b' '\0377'", expected=(b"\xff|\x1f7|\xff",b"",0))
    yield Case("printf-v-nul", br"printf -v v 'a\0b\377'; printf '<%s>' \"$v\"".replace(b'\\"',b'"'), expected=(b"<a>",b"",0))
    yield Case("printf-v-ref", b'declare -n ref=value; printf -v ref "%s" "$1"; printf "<%s>" "$value"', args=(raw,), expected=(b"<"+raw+b">",b"",0))
    yield Case("printf-q", b"printf '%q|%Q|%#q|%.3s|%8s' \"$1\" \"$1\" \"$1\" \"$1\" \"$1\"", args=(raw,))
    yield Case("printf-numeric-complex", b"printf '%+08d|%#x|%.*f|%e|%g|%c' 23 255 3 1.25 2 3.5 X")
    yield Case("printf-invalid-conversion", b'printf "head%\xffTAIL" arg; printf "|%s" "$?"')
    yield Case("printf-empty-vs-missing-number", b'printf "%d|%u|%x|%g" "" "" "" ""; s=$?; printf "|%s|%d" "$s"')
    yield Case("printf-format-escaped-quote", br'''printf '\"|%b' '\"' ''',expected=(b'"|\\"',b'',0))
    yield Case("printf-invalid-number", b'printf "%d|%d" "$1" 3; printf "|%s" "$?"', args=(b"\xff",))
    yield Case("printf-unicode", br"printf '\u00e9|%b' '\U0001F600'; echo -e '\u00e9'")
    yield Case("read-high-native-fd", b'exec 300<./input; IFS= read -r -u 300 v; printf "<%s>" "$v"; /usr/bin/cat <&300',files=((b'input',b'\xff\n\xfe',0o600),),expected=(b'<\xff>\xfe',b'',0))
    yield Case("read-raw-controls", b'IFS= read -r v; s=$?; printf "<%s>|%s" "$v" "$s"; /usr/bin/cat', stdin=b"a\xff\0\x01\x03\x04\x1b\xfe\nrest", expected=(b"<a\xff\x01\x03\x04\x1b\xfe>|0rest",b"",0))
    for count in (0,1,2,3):
        for flag in (b"-n",b"-N"):
            yield Case("read-count-"+flag.decode()+str(count), b'IFS= read -r '+flag+b' '+str(count).encode()+b' v; s=$?; printf "<%s>|%s|" "$v" "$s"; /usr/bin/cat', stdin=b"\xc3X\xfeY\nrest")
    for data in (b"\xc3",b"\xe2\x82",b"\xc3\0Z",b"\xc3\nZ",b"\xe2\x82XQ",b"\xe0\x80Z",b"\xed\xa0\xbfZ"):
        yield Case("read-incomplete-"+data.hex(), b'IFS= read -r -N 1 v; s=$?; printf "<%s>|%s|" "$v" "$s"; /usr/bin/cat', stdin=data)
    for flag in (b"-n",b"-N"):
        for count in (2,3):
            yield Case("read-malformed-nul-"+flag.decode()+str(count), b'IFS= read -r '+flag+b' '+str(count).encode()+b' v; s=$?; printf "<%s>|%s|" "$v" "$s"; /usr/bin/cat',stdin=b"\xc3\0Z")
    yield Case("read-byte-delimiter", b'IFS= read -r -d "$1" v; printf "<%s>" "$v"; /usr/bin/cat', stdin=b"ab\xc3\xa9rest",args=(b"\xc3\xa9",),expected=(b"<ab>\xa9rest",b"",0))
    yield Case("read-nul-delimiter", b'IFS= read -r -d "" v; printf "<%s>" "$v"; /usr/bin/cat',stdin=b"\xff\0\xfe",expected=(b"<\xff>\xfe",b"",0))
    for line in (b"a,b,\n",b"a,,\n",b"a, ,\n",b" a , b , \n",b"a\\,b,c\n",b"a\\ b c\n"):
        yield Case("read-ifs-"+line.hex(), b"IFS=', '; read a b; printf '<%s><%s>' \"$a\" \"$b\"",stdin=line)
    yield Case("read-raw-ifs", b'IFS="$1" read -r a b; printf "<%s><%s>" "$a" "$b"',args=(b"\xff",),stdin=b"a\xffb\xffc\n",expected=(b"<a><b\xffc>",b"",0))
    yield Case("read-reply-whitespace", b'read -r; printf "<%s>" "$REPLY"',stdin=b"  \xff  \n",expected=(b"<  \xff  >",b"",0))
    yield Case("read-nameref-index", b'declare -n ref="a[2]"; IFS= read -r ref; printf "<%s>" "${a[2]}"',stdin=raw+b"\n",expected=(b"<"+raw+b">",b"",0))
    yield Case("read-timeout-partial-unit", b'IFS= read -r -t 0.1 v; s=$?; printf "<%s>|%s" "$v" "$s"', stdin=b"\xff\xc3", hold_open=True, expected=(b"<\xff\xc3>|142",b"",0))
    yield Case("read-timeout-N-partial-unit", b'IFS= read -r -N 1 -t 0.1 v; s=$?; printf "<%s>|%s" "$v" "$s"', stdin=b"\xc3", hold_open=True)
    yield Case("read-timeout-backslash", b'IFS= read -t 0.1 v; s=$?; printf "<%s>|%s" "$v" "$s"', stdin=b"a\\", hold_open=True)
    yield Case("read-t0-noassign", b'v=old; read -t 0 v; s=$?; printf "<%s>|%s|" "$v" "$s"; /usr/bin/cat',stdin=b"data\n",expected=(b"<old>|0|data\n",b"",0))
    yield Case("mapfile-raw-nul-controls", b'mapfile a; printf "<%s>" "${a[@]}"',stdin=b"\xff\x03\x04\npre\0post\n\xfe",expected=(b"<\xff\x03\x04\n><pre><\xfe>",b"",0))
    yield Case("mapfile-callback-status", b'cb() { printf "cb<%s><%s>" "$1" "$2"; return 7; }; mapfile -t -C cb -c 1 a; s=$?; printf "|%s|" "$s"; printf "<%s>" "${a[@]}"',stdin=b"\xff\n\xfe\n",expected=(b"cb<0><\xff>cb<1><\xfe>|0|<\xff><\xfe>",b"",0))
    yield Case("mapfile-origin-n", b'a=(old keep); mapfile -t -O 1 -n 1 a; printf "<%s>" "${a[@]}"; /usr/bin/cat',stdin=b"\xff\n\xfe\n",expected=(b"<old><\xff>\xfe\n",b"",0))
    yield Case("set-positionals", b'set -- "$1" "$2"; printf "<%s>" "$@"',args=(raw,b"\xfe"),expected=(b"<"+raw+b"><\xfe>",b"",0))
    yield Case("eval-raw-source", b'eval "$1"', args=(b"printf '<%s>' '\xff\xfe'",),expected=(b"<\xff\xfe>",b"",0))
    yield Case("dot-raw-name-data", b'. ./source_\xff "$1"',args=(raw,),files=((b"source_\xff",b'printf "<%s><%s>" "$1" "${BASH_SOURCE[0]}"',0o600),),expected=(b"<"+raw+b"><./source_\xff>",b"",0))
    child=b"#!/usr/bin/python3\nimport os,sys,json\nprint(json.dumps({'argv':[os.fsencode(a).hex() for a in sys.argv[1:]],'env':os.environb.get(b'RAW',b'').hex(),'opaque':os.environb.get(b'BAD_\\xff',b'').hex()},sort_keys=True,separators=(',',':')))\n"
    yield Case("export-exec-native", b'export RAW="$1"; exec ./child "$1"',args=(raw,),files=((b"child",child,0o700),))
    yield Case("exec-enoexec-native", b'exec ./script_\xff "$1"',args=(raw,),files=((b"script_\xff",b'printf "<%s><%s>" "$1" "$0"',0o700),))
    yield Case("exec-native-raw-argv0-env", b'export RAW="$1"; exec -a "$2" ./native-child "$1"', args=(raw,b"argv0_\xff"),env=((b"BAD_\xff",b"opaque\xfe"),),files=((b"native-child",NATIVE,0o700),),expected=(frame(b"argv0_\xff")+frame(raw)+frame(raw)+frame(b"opaque\xfe"),b"",0))
    yield Case("exec-native-empty-env", b'export RAW="$1"; exec -c -a "$2" ./native-child "$1"', args=(raw,b"argv0_\xff"),env=((b"BAD_\xff",b"opaque\xfe"),),files=((b"native-child",NATIVE,0o700),),expected=(frame(b"argv0_\xff")+frame(raw)+frame(None)+frame(None),b"",0))
    yield Case("exec-native-login-argv0", b'exec -l -a "$1" ./native-child value',args=(b"login\xff",),files=((b"native-child",NATIVE,0o700),),expected=(frame(b"-login\xff")+frame(b"value")+frame(None)+frame(None),b"",0))
    yield Case("export-quoted-assignment", b'export "$1"; ./child',args=(b"RAW="+raw,),files=((b"child",child,0o700),))
    yield Case("export-array-identifier", b'export a[0]=x; printf "|%s|%s" "$?" "${a[0]}"',expected=(b'|1|',b"probe: line 1: export: `a[0]': not a valid identifier\n",0))
    yield Case("export-quoted-ref-element", b'declare -n r="a[0]"; export "$1"; s=$?; printf "<%s>|%s" "${a[0]}" "$s"',args=(b'r='+raw,),expected=(b'<'+raw+b'>|0',b"probe: line 1: export: `a[0]': not a valid identifier\n",0))
    yield Case("export-bad-identifier", b'export "$1"; printf "|%s" "$?"',args=(b"bad\xff="+raw,))
    yield Case("unset-opaque-env", b'unset "$1"; ./child',args=(b"BAD_\xff",),env=((b"BAD_\xff",raw),),files=((b"child",child,0o700),))
    yield Case("unset-ref-raw-index", b'declare -A a; a["$1"]=value; declare -n ref=\'a["$1"]\'; unset ref; printf "%s" "${#a[@]}"',args=(raw,),expected=(b"0",b"",0))
    yield Case("getopts-ref-raw-optarg", b'declare -n ref=result; getopts a:b ref -a "$1"; printf "<%s><%s><%s>" "$result" "$OPTARG" "$OPTIND"',args=(raw,),expected=(b"<a><"+raw+b"><3>",b"",0))
    yield Case("getopts-byte-option", b'getopts "$1" v "$2"; printf "<%s><%s><%s>" "$v" "$OPTARG" "$OPTIND"',args=(b":\xff:",b"-\xff"+raw),expected=(b"<\xff><"+raw+b"><2>",b"",0))
    yield Case("getopts-subscripted-ref", b'declare -n ref="a[2]"; getopts a: ref -a "$1"; printf "<%s><%s><%s>" "${a[2]}" "$OPTARG" "$ref"', args=(raw,), expected=(b"<a><"+raw+b"><a>",b"",0))
    yield Case("getopts-optarg-ref", b'declare -n OPTARG=result; getopts a: v -a "$1"; printf "<%s><%s>" "$OPTARG" "$result"', args=(raw,), expected=(b"<"+raw+b"><"+raw+b">",b"",0))
    yield Case("mapfile-callback-consumes-input", b'cb() { IFS= read -r discarded; printf "<%s>" "$discarded"; }; mapfile -t -C cb -c 1 a; printf "|"; printf "<%s>" "${a[@]}"',stdin=b"\xff\nconsumed\n\xfe\nlast\n",expected=(b"<consumed><last>|<\xff><\xfe>",b"",0))
    yield Case("unset-readonly-continues", b'readonly first=one; second=two; unset first second; s=$?; printf "<%s><%s>|%s" "$first" "${second-unset}" "$s"')
    yield Case("read-readonly-continues", b'readonly first=one; read -r first second; s=$?; printf "<%s><%s>|%s" "$first" "$second" "$s"',stdin=b"change two\n")
    yield Case("export-readonly-continues", b'readonly first=one; export first=change second="$1"; s=$?; printf "<%s><%s>|%s" "$first" "$second" "$s"',args=(raw,))
    yield Case("getopts-optind-same-two", b'set -- skip -xyz -abc; OPTIND=2; getopts xyzabc v; printf "%s:" "$v"; OPTIND=2; getopts xyzabc v; printf "%s:%s" "$v" "$OPTIND"',expected=(b'x:y:2',b'',0))
    yield Case("getopts-optind-change-midcluster", b'set -- -xyz -abc; getopts xyzabc v; printf "%s:" "$v"; OPTIND=2; getopts xyzabc v; printf "%s:%s" "$v" "$OPTIND"; getopts xyzabc v; printf ":%s:%s" "$v" "$OPTIND"; getopts xyzabc v; printf ":%s:%s" "$v" "$OPTIND"',expected=(b'x:y:2:z:3:?:3',b'',0))
    yield Case("getopts-fresh-argv-midcluster", b'getopts xyzabc v -xyz; printf "%s:" "$v"; getopts xyzabc v -abc; printf "%s:%s" "$v" "$OPTIND"',expected=(b'x:b:1',b'',0))
    yield Case("getopts-reset-optind", b'set -- -abc; getopts abc v; printf "%s" "$v"; OPTIND=1; getopts abc v; printf "%s|%s" "$v" "$OPTIND"',expected=(b"aa|1",b"",0))


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(binary, case, locale, directory):
    for name, data, mode in case.files:
        path = os.path.join(os.fsencode(directory), name)
        with open(path, "wb") as stream: stream.write(data)
        os.chmod(path, mode)
    env = {b"PATH":b"/usr/bin:/bin",b"HOME":os.fsencode(directory),b"LC_ALL":locale,b"TERM":b"dumb"}
    env.update(case.env)
    argv=[os.fsencode(binary),b"--noprofile",b"--norc",b"-c",case.source,b"probe",*case.args]
    started=time.monotonic()
    input_fd, held_writer = os.pipe() if case.hold_open else (None, None)
    p=subprocess.Popen(argv,cwd=directory,env=env,stdin=input_fd if case.hold_open else subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    if case.hold_open:
        os.close(input_fd)
        os.write(held_writer, case.stdin)
    timed_out=False
    try:
        out,err=p.communicate(None if case.hold_open else case.stdin,timeout=5)
    except subprocess.TimeoutExpired:
        timed_out=True
        os.killpg(p.pid,signal.SIGKILL)
        out,err=p.communicate(timeout=3)
    finally:
        if held_writer is not None: os.close(held_writer)
    # All journeys are finite and synchronously wait for children. No background
    # test is allowed to silently leave an owned group behind.
    try:
        os.killpg(p.pid,0)
    except ProcessLookupError:
        survivor=False
    else:
        survivor=True
        os.killpg(p.pid,signal.SIGKILL)
    effects={}
    for path in sorted(Path(directory).iterdir()):
        if path.is_file(): effects[os.fsencode(path.name).hex()]=path.read_bytes().hex()
    return dict(stdout=out.hex(),stderr=err.hex(),status=p.returncode,files=effects,timeout=timed_out,survivor=survivor,pid=p.pid,pgid=p.pid,elapsed=time.monotonic()-started)


def terminal_cases():
    yield Case('tty-read-raw-silent',b'IFS= read -s -r -p ">" v; s=$?; printf "<%s>|%s" "$v" "$s"',stdin=b"a\xff\xfe\n",expected=(b"<a\xff\xfe>|0",b">",0))
    yield Case('tty-read-incremental',b'IFS= read -s -r -n 1 -p ">" v; IFS= read -s -r -n 1 w; printf "<%s><%s>" "$v" "$w"',stdin=b"\xc3X\xfe\n")
    yield Case('tty-read-n-eot-data',b'IFS= read -s -r -n 1 -p ">" v; s=$?; printf "<%s>|%s" "$v" "$s"',stdin=b"\x04",expected=(b"<\x04>|0",b">",0))
    yield Case('tty-read-eof',b'IFS= read -s -r -p ">" v; s=$?; printf "<%s>|%s" "$v" "$s"',stdin=b"\x04",expected=(b"<>|1",b">",0))


def run_terminal(binary, case, locale, directory):
    master,slave=pty.openpty()
    before=termios.tcgetattr(slave)
    env={b'PATH':b'/usr/bin:/bin',b'HOME':os.fsencode(directory),b'LC_ALL':locale,b'TERM':b'dumb'}
    p=subprocess.Popen([os.fsencode(binary),b'--noprofile',b'--norc',b'-c',case.source,b'probe'],cwd=directory,env=env,stdin=slave,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True,preexec_fn=lambda:fcntl.ioctl(0,termios.TIOCSCTTY,0))
    header=b''; timed_out=False
    try:
        if select.select([p.stderr],[],[],3)[0]: header=os.read(p.stderr.fileno(),1)
        if header==b'>': os.write(master,case.stdin)
        out,err=p.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        timed_out=True;os.killpg(p.pid,signal.SIGKILL);out,err=p.communicate(timeout=3)
    after=termios.tcgetattr(slave)
    os.close(slave)
    terminal_output=b''
    while select.select([master],[],[],0)[0]:
        try: chunk=os.read(master,4096)
        except OSError: break
        if not chunk: break
        terminal_output+=chunk
    os.close(master)
    try: os.killpg(p.pid,0)
    except ProcessLookupError: survivor=False
    else: survivor=True;os.killpg(p.pid,signal.SIGKILL)
    return dict(stdout=out.hex(),stderr=(header+err).hex(),status=p.returncode,files={},timeout=timed_out,survivor=survivor,pid=p.pid,pgid=p.pid,terminal_output=terminal_output.hex(),terminal_restored=before==after)


def observable(result):
    return {key:result[key] for key in ('stdout','stderr','status','files','timeout','survivor','terminal_output','terminal_restored') if key in result}


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--oracle',type=Path,default=ROOT/'target/marsh-evidence/bash-jobs/gnu-bash-5.3.20')
    ap.add_argument('--pristine',type=Path)
    ap.add_argument('--prebyte',type=Path)
    ap.add_argument('--candidate',type=Path)
    ap.add_argument('--evidence',type=Path,required=True)
    ap.add_argument('--filter',default='')
    ns=ap.parse_args(); ns.evidence.mkdir(parents=True,exist_ok=True)
    global NATIVE
    NATIVE=build_native(ns.evidence)
    if sha(ns.oracle)!=ORACLE_SHA: raise SystemExit('frozen GNU 5.3.20 SHA mismatch')
    binaries={k:getattr(ns,k).resolve() for k in ('oracle','pristine','prebyte','candidate') if getattr(ns,k)}
    identity={'binaries':{k:{'path':str(v),'sha256':sha(v)} for k,v in binaries.items()},'harness_sha256':sha(__file__)}
    (ns.evidence/'identity.json').write_text(json.dumps(identity,indent=2)+'\n')
    counts={k:{'exact':0,'different':0,'assertion_failures':0} for k in binaries}
    with tempfile.TemporaryDirectory(prefix='marsh-byte-builtins-') as tmp, (ns.evidence/'observations.jsonl').open('w') as log:
        for locale in (b'C',b'C.utf8'):
            for case in [*cases(), *terminal_cases()]:
                if ns.filter and ns.filter not in case.name: continue
                # Paths are identical for every implementation. Restore inputs
                # between runs rather than normalize path bytes in outputs.
                work=Path(tmp)/'work'; work.mkdir(exist_ok=True)
                reference=None
                for kind,binary in binaries.items():
                    for path in work.iterdir(): path.unlink()
                    record=(run_terminal if case.name.startswith('tty-') else run)(binary,case,locale,work)
                    expectation=case.expected
                    if case.name=='exec-enoexec-native':
                        expectation=(b'<'+case.args[0]+b'><'+os.fsencode(work)+b'/script_\xff>',b'',0)
                    asserted=expectation is None or (bytes.fromhex(record['stdout']),bytes.fromhex(record['stderr']),record['status'])==expectation
                    if not asserted: counts[kind]['assertion_failures']+=1
                    if reference is None: reference=observable(record)
                    exact=observable(record)==reference
                    counts[kind]['exact' if exact else 'different']+=1
                    log.write(json.dumps({'case':case.name,'locale':locale.decode(),'binary':kind,'source_hex':case.source.hex(),'asserted':asserted,'exact':exact,**record})+'\n'); log.flush()
    (ns.evidence/'summary.json').write_text(json.dumps(counts,indent=2)+'\n')
    print(json.dumps(counts,indent=2))
    return int(bool(counts['oracle']['assertion_failures']) or bool(ns.candidate and (counts['candidate']['different'] or counts['candidate']['assertion_failures'])))

if __name__=='__main__': raise SystemExit(main())
