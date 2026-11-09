#!/usr/bin/env python3
"""Stage one fixture directory into a project folder.

    stage.py FIXTURE DEST [--with FIXTURE]...   e.g. stage.py stack-a /private/tmp/nfx-run/p1 --with devserver

Copies FIXTURE (symlinks followed, modes kept) into DEST, which must be absent
or empty, then each --with FIXTURE over it (files must not collide). Then:
  env.template        -> .env        (the git-ignored environment file)
  gitignore.template  -> .gitignore
  stage-hook.sh       is not copied; it runs in DEST afterwards when present
                      (build-ctx uses it to write its 50 MiB random payload).
A fixture in NPM_FIXTURES has no committed node_modules: the first stage runs
`npm ci --ignore-scripts` in the fixture directory (needs npm and the network),
and again whenever its package-lock.json changes.
Prints DEST. Synthetic data only; nothing here reads the environment.
"""
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
RENAMES = {"env.template": ".env", "gitignore.template": ".gitignore"}
NPM_FIXTURES = ("tc", "devserver")


def ensure_installed(source: pathlib.Path) -> None:
    """`npm ci` in a fixture directory whose node_modules is not committed."""
    if source.name not in NPM_FIXTURES:
        return
    lock = hashlib.sha256((source / "package-lock.json").read_bytes()).hexdigest()
    stamp = source / "node_modules" / ".marsh-installed-lock"
    if stamp.is_file() and stamp.read_text().strip() == lock:
        return
    subprocess.run(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], cwd=source, check=True)
    stamp.write_text(lock + "\n")


def stage(fixture: str, dest: pathlib.Path, overlays: tuple[str, ...] = ()) -> pathlib.Path:
    if dest.exists() and any(dest.iterdir()):
        raise SystemExit(f"stage: {dest} exists and is not empty")
    for name in (fixture, *overlays):
        source = HERE / name
        if not source.is_dir() or source.parent != HERE:
            raise SystemExit(f"stage: unknown fixture {name!r}")
        ensure_installed(source)
        before = {p.relative_to(dest) for p in dest.rglob("*") if p.is_file()} if dest.exists() else set()
        shutil.copytree(source, dest, symlinks=False, dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns("stage-hook.sh", "generate.py", "__pycache__"),
                        copy_function=lambda src, dst: _copy(src, dst, before, dest))
        for old, new in RENAMES.items():
            if (dest / old).exists():
                (dest / old).rename(dest / new)
        hook = source / "stage-hook.sh"
        if hook.exists():
            subprocess.run(["sh", str(hook)], cwd=dest, check=True, env={**os.environ, "NFX_FIXTURE": fixture})
    return dest


def _copy(src: str, dst: str, before: set, dest: pathlib.Path) -> None:
    if pathlib.Path(dst).relative_to(dest) in before:
        raise SystemExit(f"stage: overlay collides with {pathlib.Path(dst).relative_to(dest)}")
    shutil.copy2(src, dst)


if __name__ == "__main__":
    args = sys.argv[1:]
    overlays = []
    while "--with" in args:
        i = args.index("--with")
        overlays.append(args[i + 1])
        del args[i:i + 2]
    if len(args) != 2:
        raise SystemExit(__doc__)
    print(stage(args[0], pathlib.Path(args[1]), tuple(overlays)))
