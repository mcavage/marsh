#!/usr/bin/env python3
"""Place, drive and remove the controlled VM peer in ONE disposable project's VM.

  peer_install.py install --vm VM --script SCRIPT.json [--home MARSH_HOME --project PROJECT]
  peer_install.py release --vm VM NAME              # touch /run/nfx/NAME (a wait_for_file)
  peer_install.py log     --vm VM                   # print /run/nfx/peer.log (JSON lines)
  peer_install.py remove  --vm VM [--home MARSH_HOME]

install puts peer.py and the script in /run/nfx of the VM (`sbx exec -i VM sudo
...`; /run is a tmpfs, so repeat install after a VM restart) and, with --home and
--project, writes PROJECT (the project root path the daemon matches) to
$MARSH_HOME/test-holds/agent-override: the file that makes a breaks-build daemon
launched explicitly with `fault-agent-override` (H2) run the peer instead of the
engine agent for that project only. remove deletes both. Refuses a VM that is
not a harness resource (name starting `marsh-xnfx01-` or `nfx-`); the harness
must also have checked the VM's name and UUID against its ownership map.
"""
import argparse
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
DIR = "/run/nfx"


def guard(vm: str) -> None:
    if not (vm.startswith("marsh-xnfx01-") or vm.startswith("nfx-")):
        raise SystemExit(f"peer_install: refusing {vm!r}: not a harness VM (marsh-xnfx01-* or nfx-*)")


def vm_sh(args, script: str, data: bytes | None = None) -> subprocess.CompletedProcess:
    cmd = [args.sbx, "exec", *(["-i"] if data is not None else []), args.vm, "sudo", "sh", "-c", script]
    done = subprocess.run(cmd, input=data, capture_output=True)
    if done.returncode != 0:
        raise SystemExit(f"peer_install: `{script}` failed: {done.stderr.decode(errors='replace').strip()}")
    return done


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("action", choices=["install", "release", "log", "remove"])
    parser.add_argument("name", nargs="?")
    parser.add_argument("--vm", required=True)
    parser.add_argument("--script")
    parser.add_argument("--home")
    parser.add_argument("--project")
    parser.add_argument("--sbx", default="sbx")
    args = parser.parse_args()
    guard(args.vm)
    if args.action == "install":
        if not args.script:
            parser.error("install needs --script")
        vm_sh(args, f"mkdir -p {DIR} && chmod 755 {DIR} && rm -f {DIR}/peer.log {DIR}/go-*")
        vm_sh(args, f"cat > {DIR}/peer.py && chmod 755 {DIR}/peer.py", (HERE / "peer.py").read_bytes())
        vm_sh(args, f"cat > {DIR}/peer-script.json", pathlib.Path(args.script).read_bytes())
        if args.home and args.project:
            holds = pathlib.Path(args.home) / "test-holds"
            holds.mkdir(parents=True, exist_ok=True)
            (holds / "agent-override").write_text(args.project + "\n")
        print(f"installed in {args.vm}:{DIR}")
    elif args.action == "release":
        if not args.name:
            parser.error("release needs NAME")
        vm_sh(args, f"touch {DIR}/{args.name}")
    elif args.action == "log":
        sys.stdout.write(vm_sh(args, f"cat {DIR}/peer.log").stdout.decode())
    else:
        vm_sh(args, f"rm -rf {DIR}")
        if args.home:
            (pathlib.Path(args.home) / "test-holds" / "agent-override").unlink(missing_ok=True)
        print(f"removed {DIR} in {args.vm}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
