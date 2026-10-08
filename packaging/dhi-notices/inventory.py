#!/usr/bin/env python3
"""Read-only installed DHI image inventory; run with network disabled, no user mounts.

This emits installed package versions, exact common binary hashes, and actual
package copyright/license bytes. It executes only dpkg-query, never agent or npm
code. Results are evidence inputs, not a claim of complete legal compliance.
"""
import base64
import hashlib
import json
from pathlib import Path
import subprocess

BINARIES = [
    '/usr/bin/buildkitd', '/usr/bin/containerd', '/usr/bin/containerd-shim-runc-v2',
    '/usr/bin/ctr', '/usr/bin/docker', '/usr/bin/dockerd', '/usr/bin/docker-proxy',
    '/usr/bin/gh', '/usr/bin/runc', '/usr/bin/node', '/usr/lib/go/bin/go', '/usr/local/bin/node',
    '/usr/libexec/docker/cli-plugins/docker-buildx',
    '/usr/libexec/docker/cli-plugins/docker-compose',
    '/usr/local/bin/claude', '/usr/local/bin/codex', '/usr/bin/claude', '/usr/bin/codex',
]

def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def main():
    versions = subprocess.check_output(['dpkg-query', '-W', '-f=${Package}\t${Version}\n'], text=True)
    packages = []
    texts = {}
    for line in versions.splitlines():
        name, version = line.split('\t', 1)
        directory = Path('/usr/share/doc') / name
        notices = []
        if directory.is_dir():
            for candidate in sorted(directory.iterdir()):
                # Exact installed-document whitelist (not a recursive prefix
                # regex): license.js/license.go/notices.rs never match. Recursive
                # archive collectors share notice_rules.py instead.
                if candidate.name.lower() not in {'copyright', 'license', 'license.txt', 'notice', 'notice.txt', 'copying'}:
                    continue
                resolved = candidate.resolve(strict=True)
                if not resolved.is_relative_to('/usr/share/doc') or not resolved.is_file():
                    raise ValueError('package notice escaped the installed documentation tree')
                data = resolved.read_bytes()
                if len(data) > 16 * 1024 * 1024:
                    raise ValueError('oversize package notice')
                sha = hashlib.sha256(data).hexdigest()
                notices.append({'path': str(candidate), 'resolved': str(resolved), 'sha256': sha})
                texts[sha] = base64.b64encode(data).decode('ascii')
        packages.append({'package': name, 'version': version, 'notices': notices})
    binaries = []
    missing = []
    for name in BINARIES:
        path = Path(name)
        if not path.exists():
            missing.append(name)
            continue
        resolved = path.resolve(strict=True)
        if not resolved.is_file():
            raise ValueError('expected regular binary entry')
        binaries.append({'path': name, 'resolved': str(resolved), 'sha256': digest(resolved)})
    print(json.dumps({'schema':'marsh.dhi-installed-inventory/v1', 'packages':packages,
        'binaries':binaries, 'missing_binary_paths':missing, 'notice_texts_base64':texts,
        'scope':'Actual installed public DHI package documentation and explicit executable paths; no agent invocation, network, or host profile reads.'}, sort_keys=True))

if __name__ == '__main__':
    main()
