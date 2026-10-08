#!/usr/bin/env python3
"""Read only two fixed public-image legal files; never inspect host/user profiles."""
import base64
import hashlib
import json
import os
import stat

PATHS = ('/opt/docker/.license.txt', '/opt/docker/sbom/clipboard-bridge/.spdx.clipboard-bridge.json')
rows = []
for path in PATHS:
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 1024**2:
            raise ValueError('unbounded public image legal file')
        data = source.read(1024**2 + 1)
        if len(data) != metadata.st_size:
            raise ValueError('public image legal file changed')
    rows.append({'path': path, 'sha256': hashlib.sha256(data).hexdigest(),
                 'size': len(data), 'mode': stat.S_IMODE(metadata.st_mode),
                 'uid': metadata.st_uid, 'gid': metadata.st_gid,
                 'bytes_base64': base64.b64encode(data).decode()})
print(json.dumps({'schema': 'marsh.public-image-legal-files/v1', 'files': rows}, sort_keys=True))
