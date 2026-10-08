"""Require an explicitly selected protocol binary; never adopt a build cache."""
import os
from pathlib import Path


def required_binary(variable: str) -> Path:
    value = os.environ.get(variable)
    if not value:
        raise RuntimeError(f"set {variable} to the protocol binary built for this run")
    path = Path(value).resolve(strict=True)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"{variable} must name an executable regular file: {path}")
    return path
