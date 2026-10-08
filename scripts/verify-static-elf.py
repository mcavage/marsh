#!/usr/bin/env python3
"""Reject a wrong-architecture or dynamically linked native transport helper."""
import argparse
import struct
from pathlib import Path


def verify(path: Path, machine: int) -> None:
    with path.open('rb') as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:7] != b'\x7fELF\x02\x01\x01':
            raise ValueError('helper must be a little-endian ELF64 executable')
        if struct.unpack_from('<H', header, 16)[0] not in (2, 3):
            raise ValueError('helper must be an executable or static PIE, not an object file')
        if struct.unpack_from('<H', header, 18)[0] != machine:
            raise ValueError('helper architecture differs from the selected platform')
        offset = struct.unpack_from('<Q', header, 32)[0]
        entry_size, count = struct.unpack_from('<HH', header, 54)
        size = path.stat().st_size
        if entry_size != 56 or not 1 <= count <= 1024 or offset + entry_size * count > size:
            raise ValueError('invalid helper program header table')
        for number in range(count):
            stream.seek(offset + number * entry_size)
            kind, _, start, _, _, length, _, _ = struct.unpack('<IIQQQQQQ', stream.read(entry_size))
            if start + length > size:
                raise ValueError('helper segment exceeds the executable')
            if kind == 3:
                raise ValueError('helper has PT_INTERP; dynamic loaders are not portable to arbitrary Kits')
            if kind == 2:
                if length > 1024 * 1024 or length % 16:
                    raise ValueError('invalid helper dynamic segment')
                stream.seek(start)
                for entry in range(length // 16):
                    tag, _ = struct.unpack('<QQ', stream.read(16))
                    if tag == 1:
                        raise ValueError('helper has DT_NEEDED shared-library dependencies')
                    if tag == 0:
                        break


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--architecture', choices=('arm64', 'amd64'), default='arm64')
    args = parser.parse_args()
    try:
        verify(args.binary, 183 if args.architecture == 'arm64' else 62)
    except (OSError, ValueError, struct.error) as error:
        raise SystemExit(f'static helper verification failed: {error}') from error
