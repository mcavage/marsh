#!/usr/bin/env python3
"""Byte-exact GNU differential matchers, through shell callers or source-bound driver.

--candidate is a real shell. --driver is explicitly only supporting matcher
coverage, not a shell qualification. No text decoding of payloads or filenames.
Oracle must be GNU 5.3.20; every observation retains stdout/stderr/status/timing.
Each invocation has its own tracked session, timeout cleanup and survivor check.
"""
from __future__ import annotations
import argparse
import dataclasses
import hashlib
import json
import os
from pathlib import Path
import random
import signal
import subprocess
import tempfile
import time


@dataclasses.dataclass
class Case:
    name: str
    op: str
    value: bytes
    pieces: list[tuple[bool, bytes]]
    mode: str = "C"
    nocase: bool = False
    kind: str = ""
    replacement: bytes = b"<&$\\\xff>"
    dotglob: bool = False
    extglob: bool = True

    def request(self, cwd: bytes) -> dict:
        return dict(op=self.op, value=list(self.value),
                    pieces=[dict(literal=q, bytes=list(p)) for q, p in self.pieces],
                    mode=self.mode, nocase=self.nocase, kind=self.kind,
                    replacement=list(self.replacement), cwd=list(cwd),
                    dotglob=self.dotglob, extglob=self.extglob)

    def invocation(self) -> tuple[bytes, list[bytes]]:
        args = [self.value] + [p for _, p in self.pieces]
        word = b"".join((b'"${%d}"' if quoted else b'${%d}') % (i + 2)
                        for i, (quoted, _) in enumerate(self.pieces))
        setup = b"v=$1; IFS=; "
        setup += b"shopt -s extglob; " if self.extglob else b"shopt -u extglob; "
        setup += b"shopt -s nocasematch nocaseglob; " if self.nocase else b"shopt -u nocasematch nocaseglob; "
        if self.op == "match":
            return setup + b"[[ $v == " + word + b" ]]", args
        if self.op == "case":
            return setup + b"printf '%s' \"${v" + self.kind.encode() + b"}\"", args
        if self.op == "regex":
            return (setup + b"[[ $v =~ " + word +
                    b" ]]; s=$?; if ((s==0)); then printf '%s\\0' \"${BASH_REMATCH[@]}\"; fi; exit $s"), args
        if self.op == "remove":
            return setup + b"printf '%s' \"${v" + self.kind.encode() + word + b"}\"", args
        if self.op == "replace":
            args.append(self.replacement)
            setup += b"shopt -u patsub_replacement; r=${%d}; " % len(args)
            return setup + b"printf '%s' \"${v/" + self.kind.encode() + word + b"/$r}\"", args
        if self.op == "glob":
            setup += b"shopt -s nullglob; "
            setup += b"shopt -s dotglob; " if self.dotglob else b"shopt -u dotglob; "
            return setup + b"a=( " + word + b" ); if ((${#a[@]})); then printf '%s\\0' \"${a[@]}\"; fi; exit 0", args
        raise ValueError(self.op)


def corpus(full: bool) -> list[Case]:
    cases = []
    values = [b"", b"a", b"ab", b"abc", b"aaa", b"aba", b"aaab", b".a", b"\n", b"a\nb",
              b"\xff", b"\xfe", b"\xef\xbf\xbd", b"\xc3\xa9", b"\xc3X", b"A\xffB", b"*?[]\\"]
    patterns = [b"", b"*", b"?", b"??", b"a*", b"*a", b"*a*b", b"[!a]", b"[]]", b"[][]", b"[]-a]",
                b"[z-a]", b"[z-aX]", b"[[:alpha:]]", b"[[:print:]]", b"[[:digit:]]", b"[![:alpha:]]",
                b"@(a|ab)", b"!(a)", b"!(a*)", b"a!(a)a", b"!(a|b)*", b"*(a|b)", b"+(a|aa)",
                b"?(!(a)|b)", b"!(@(a|b)|+(c))", b"!()", b"*(|a)", b"!(a)b", b"[", b"\\*", b"\\"]
    rng = random.Random(0xB17E)
    for mode in ("C", "C.utf8"):
        for i, (value, pattern) in enumerate((v, p) for v in values for p in patterns):
            if not full and rng.randrange(5):
                continue
            cases.append(Case(f"glob-predicate-{mode}-{i}", "match", value, [(False, pattern)], mode))
        for byte in range(1, 256):
            b = bytes([byte])
            # Quoted literal is essential for all metacharacter bytes, and must
            # distinguish every byte from the Unicode replacement character.
            cases.append(Case(f"literal-all255-{mode}-{byte}", "match", b, [(True, b)], mode))
            if full or byte >= 128:
                cases.append(Case(f"distinct-all255-{mode}-{byte}", "match", b"\xef\xbf\xbd", [(True, b)], mode))
        for value in (b"a\xff\xfea", b"\xc3\xa9\xffa", b"a\xff\xc3\xa9", b"\xff\xc3\xa9", b"\xc3\xa9a", b"aaaa", b"", b"\nxx\n"):
            for pattern in (b"*", b"?", b"??", b"a*", b"*a", b"!(a)", b"?(a)", b"*(a)", b"@(|a)", b"@(a|aa)", b""):
                for kind in ("#", "##", "%", "%%"):
                    cases.append(Case(f"remove-{mode}-{value.hex()}-{pattern.hex()}-{kind}", "remove", value, [(False, pattern)], mode, kind=kind))
                for kind in ("", "/", "#", "%"):
                    cases.append(Case(f"replace-{mode}-{value.hex()}-{pattern.hex()}-{kind}", "replace", value, [(False, pattern)], mode, kind=kind))
        for value, pattern in [(b"ab", b"(a|ab)"), (b"aa", b"(a|aa)(a?)"), (b"aaa", b"(a*)(a*)"),
                               (b"b", b"(a)?(b)"), (b"x\xff\xfeZ", b"(\xff|\xff\xfe)"),
                               (b"\xff", b"."), (b"\xff", b"(.*)"), (b"\xef\xbf\xbd", b"(.)"),
                               (b"\xc3\xa9", b"(.)"), (b"a\nb", b"(a.b)"), (b"a\nb", b"^b"),
                               (b"[", b"[[]"), (b"]", b"[]]"), (b"a", b"["), (b"a", b"("),
                               (b"aa", b"(a)\\1"), (b"A", b"[[:lower:]]"),
                               (b"a", b"(\xff"), (b"a", b"\xff["), (b"a", b"[\xff-a]")]: 
            cases.append(Case(f"ere-{mode}-{value.hex()}-{pattern.hex()}", "regex", value, [(False, pattern)], mode))
        for byte in range(1, 256):
            cases.append(Case(f"ere-literal-all255-{mode}-{byte}", "regex", bytes([byte]), [(True, bytes([byte]))], mode))
        for value in (bytes(range(1, 256)), b"Stra\xc3\x9fe", b"\xc3\xa9\xff\xc3\x89", b"\xc3X\xfeY", b"\xc4\xb0\xc4\xb1", b"\xcf\x83\xcf\x82", b"\xe1\xba\x9e", b"\xf8\x88\x80\x80\x80a"):
            for kind in ("^^", ",,"):
                cases.append(Case(f"case-{mode}-{value.hex()}-{kind}", "case", value, [(False, b"")], mode, kind=kind))
        for value, pieces in [(b"a*b", [(False, b"a"), (True, b"*"), (False, b"b")]),
                              (b"a.b", [(False, b"(a)"), (True, b"."), (False, b"(b)")]),
                              (b"[a]", [(True, b"["), (False, b"a"), (True, b"]")]),
                              (b"*\xff", [(True, b"*"), (False, b"\xff")])]:
            for op in ("match", "regex"):
                cases.append(Case(f"provenance-{op}-{mode}-{value.hex()}", op, value, pieces, mode))
        for value, pattern in [(b"A", b"a"), (b"\xc3\x89", b"\xc3\xa9"), (b"\xff", b"\xfe"),
                               (b"\xc3\xa9", b"[[:alpha:]]"), (b"\xc3X", b"??")]:
            for op in ("match", "regex"):
                cases.append(Case(f"nocase-{op}-{mode}-{value.hex()}", op, value, [(False, pattern)], mode, nocase=True))
        for pattern in (b"*", b"?", b"??", b"*\xff*", b"*\xfe*", b"*\xef\xbf\xbd*", b"[[:alpha:]]*",
                        b"!(a*)", b"@(raw*|a*)", b"sub//*", b"*/", b"dang*", b".x*", b"[.]x*", b"\\**"):
            for dotglob in (False, True):
                cases.append(Case(f"filenames-{mode}-{pattern.hex()}-{dotglob}", "glob", b"", [(False, pattern)], mode, dotglob=dotglob))
    if full:
        # Grammar-shaped adversarial callers are generated independently of the
        # implementation; GNU, never a duplicate matcher, supplies expectations.
        rng = random.Random(90210)
        tokens = [b"a", b"b", b"?", b"*", b"[ab]", b"[!a]", b"\\\\*", b"\xff", b"\xc3\xa9", b"[[:alpha:]]"]
        def generated(depth: int) -> bytes:
            if depth and rng.randrange(3) == 0:
                return rng.choice([b"@", b"?", b"*", b"+", b"!"]) + b"(" + generated(depth - 1) + b"|" + generated(depth - 1) + b")"
            return b"".join(rng.choice(tokens) for _ in range(rng.randrange(4)))
        for i in range(2000):
            pattern = generated(3)
            value = b"".join(rng.choice([b"a", b"b", b"\xff", b"\xc3\xa9", b"*"]) for _ in range(rng.randrange(9)))
            mode = rng.choice(["C", "C.utf8"])
            for op in ("match", "remove", "replace"):
                kind = rng.choice(["#", "##", "%", "%%"]) if op == "remove" else rng.choice(["", "/", "#", "%"])
                cases.append(Case(f"generated-{op}-{i}", op, value, [(False, pattern)], mode, kind=kind))
        for op in ("match", "regex"):
            for i, pieces in enumerate([
                [(False, b"[a"), (True, b"-"), (False, b"z]")],
                [(False, b"["), (True, b"!"), (False, b"a]")],
                [(False, b"["), (True, b"^"), (False, b"a]")],
                [(False, b"[a"), (True, b"]"), (False, b"b]")],
                [(False, b"["), (True, b"["), (False, b"a]")],
                [(False, b"["), (True, b"]"), (False, b"a]")],
                [(False, b"["), (True, b"\\\\"), (False, b"a]")],
                [(False, b"["), (True, b"\xff"), (False, b"a]")],
            ]):
                for value in (b"a", b"m", b"!", b"]", b"-", b"\\\\", b"\xff"):
                    cases.append(Case(f"class-provenance-{op}-{i}-{value.hex()}", op, value, pieces))
    # GNU's terminal-star/nullable-extglob optimizations are observable, even
    # when they differ from a purely compositional regular-language model.
    # Keep these as strict GNU comparisons, not known-failure waivers.
    for mode in ("C", "C.utf8"):
        for pattern in (b"*@(|a)", b"*+(|a)", b"*!(a)", b"*!(?*)", b"*!(*)b", b"*?(a)", b"*!(?*)b"):
            for value in (b"", b"a", b"b", b"ab", b"\xc3\xa9", b"\xff"):
                name = f"terminal-nullable-{mode}-{pattern.hex()}-{value.hex()}"
                cases.append(Case(name, "match", value, [(False, pattern)], mode))
                for op, kinds in (("remove", ("#", "##", "%", "%%")), ("replace", ("", "/", "#", "%"))):
                    for kind in kinds:
                        cases.append(Case(f"{name}-{op}-{kind}", op, value, [(False, pattern)], mode, kind=kind))
    # Native fix must remain unconditional even with shopt extglob disabled.
    cases.append(Case("conditional-unconditional-extglob", "match", b"ab", [(False, b"@(a|ab)")], extglob=False))
    # Wildcard exponential traps must terminate; these are ordinary matching
    # inputs, not a unit test of an internal iteration counter.
    for size in (32, 128, 512):
        cases.append(Case(f"finite-star-{size}", "match", b"a" * size, [(False, b"*a" * 24 + b"b")]))
    return cases


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def members(sid: int) -> list[int]:
    result = []
    if Path("/proc").exists():
        for p in Path("/proc").iterdir():
            if p.name.isdigit():
                try:
                    if os.getsid(int(p.name)) == sid:
                        result.append(int(p.name))
                except ProcessLookupError:
                    pass
    return result


def invoke(argv: list[bytes], data: bytes, cwd: bytes, mode: str, timeout: float) -> dict:
    env = {b"PATH": b"/usr/bin:/bin", b"HOME": cwd, b"LC_ALL": mode.encode()}
    start = time.monotonic()
    p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         cwd=cwd, env=env, start_new_session=True)
    timed_out = False
    try:
        out, err = p.communicate(data, timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(p.pid, signal.SIGKILL)  # exclusively our new session/group
        out, err = p.communicate(timeout=2)
    survivors = members(p.pid)
    if survivors:
        os.killpg(p.pid, signal.SIGKILL)
        time.sleep(0.02)
    return dict(pid=p.pid, pgid=p.pid, status=p.returncode, stdout=out.hex(), stderr=err.hex(),
                elapsed=time.monotonic() - start, timeout=timed_out, survivors=members(p.pid))


def effects(cwd: bytes) -> list[dict]:
    result = []
    for root, dirs, files in os.walk(cwd):
        for name in sorted(dirs + files):
            path = os.path.join(root, name)
            row = dict(path=os.path.relpath(path, cwd).hex(), mode=os.lstat(path).st_mode)
            if os.path.islink(path):
                row["target"] = os.readlink(path).hex()
            elif os.path.isfile(path):
                with open(path, "rb") as f:
                    row["sha256"] = hashlib.sha256(f.read()).hexdigest()
            result.append(row)
    return sorted(result, key=lambda x: x["path"])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--oracle", required=True, type=Path)
    ap.add_argument("--candidate", action="append", default=[], type=Path)
    ap.add_argument("--driver", type=Path)
    ap.add_argument("--evidence", required=True, type=Path)
    ap.add_argument("--full", action="store_true")
    ap.add_argument("--raw-cwd", action="store_true")
    ap.add_argument("--only", default="")
    ap.add_argument("--timeout", type=float, default=3)
    args = ap.parse_args()
    args.evidence.mkdir(parents=True, exist_ok=True)
    oracle = args.oracle.resolve()
    version = subprocess.run([oracle, "--version"], capture_output=True, check=True).stdout
    if b"version 5.3.20(" not in version:
        raise SystemExit("oracle is not required GNU Bash 5.3.20")
    runners = [("oracle", oracle, False)]
    runners += [(f"candidate-{i}", p.resolve(), False) for i, p in enumerate(args.candidate)]
    if args.driver:
        runners.append(("source-driver", args.driver.resolve(), True))
    binding = None
    if args.driver:
        binding = json.loads(subprocess.check_output([args.driver.resolve(), "--source-identity"]))
        if not binding.get("registry_identities_equal_root"):
            raise AssertionError("driver is not root-lock bound")
    identity = dict(harness_sha256=sha(Path(__file__)), oracle_version=version.hex(), driver_source_binding=binding,
                    binaries={name: dict(path=str(path), sha256=sha(path)) for name, path, _ in runners},
                    driver_is_not_shell_qualification=True)
    (args.evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    counts = {name: dict(total=0, exact=0, timeouts=0, survivors=0) for name, _, _ in runners}
    failures = []
    with tempfile.TemporaryDirectory(prefix="bash-byte-matchers-") as temporary:
        cwd = os.fsencode(temporary)
        if args.raw_cwd:
            cwd += b"/cwd-\xff\xfe-\xef\xbf\xbd"
            os.mkdir(cwd)
        os.mkdir(cwd + b"/sub")
        for name in (b"a", b"ab", b"A", b"\xff", b"\xfe", b"\xef\xbf\xbd", b"\xc3\xa9", b"raw\xff*?[",
                     b"raw\xfe*?[", b"raw\xef\xbf\xbd*?[", b"*literal", b".x\xff", b"sub/x\xff", b"sub/x\xfe"):
            with open(cwd + b"/" + name, "wb") as f:
                f.write(b"payload:" + name)
        for byte in range(1, 256):
            if byte == ord("/"):
                continue  # slash is a path separator, not an entry-name byte
            with open(cwd + b"/byte-%02x-" % byte + bytes([byte]), "wb") as f:
                f.write(bytes([byte]))
        os.symlink(b"missing", cwd + b"/dang\xff")
        before = effects(cwd)
        with (args.evidence / "observations.jsonl").open("w") as log:
            for case in corpus(args.full):
                if args.only and args.only not in case.name:
                    continue
                script, operands = case.invocation()
                row = dict(case=case.name, script=script.hex(), argv=[x.hex() for x in operands], request=case.request(cwd), observations={})
                expected = None
                for name, path, driver in runners:
                    if driver:
                        obs = invoke([os.fsencode(path)], json.dumps(case.request(cwd)).encode(), cwd, case.mode, args.timeout)
                    else:
                        obs = invoke([os.fsencode(path), b"--noprofile", b"--norc", b"-c", script, b"matcher", *operands], b"", cwd, case.mode, args.timeout)
                    observable = (obs["status"], obs["stdout"], obs["stderr"])
                    if expected is None:
                        expected = observable
                    obs["exact"] = observable == expected and not obs["timeout"] and not obs["survivors"]
                    counts[name]["total"] += 1
                    counts[name]["exact"] += obs["exact"]
                    counts[name]["timeouts"] += obs["timeout"]
                    counts[name]["survivors"] += len(obs["survivors"])
                    if not obs["exact"]:
                        failures.append(dict(case=case.name, runner=name))
                    row["observations"][name] = obs
                log.write(json.dumps(row) + "\n")
                log.flush()
        after = effects(cwd)
        (args.evidence / "effects.json").write_text(json.dumps(dict(before=before, after=after, unchanged=before == after), indent=2) + "\n")
        if before != after:
            raise AssertionError("matching unexpectedly changed filesystem entries")
    summary = dict(counts=counts, failures=failures)
    (args.evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(counts, indent=2))
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
