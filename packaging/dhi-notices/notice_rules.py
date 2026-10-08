"""One small notice vocabulary for preparation collectors, not a licence inference."""

import json
from pathlib import Path, PurePosixPath
import re

RULES_PATH = Path(__file__).with_name("collected") / "notice-rules.json"
RULES = json.loads(RULES_PATH.read_text())
NAME = re.compile(RULES["name_pattern"], re.I)
CODE_SUFFIXES = frozenset(RULES["code_suffixes"])
NOTICE_DIRECTORIES = frozenset(RULES["notice_directories"])
DATA_SUFFIXES = frozenset(RULES["data_suffixes"])
FIXTURE_DIRECTORIES = frozenset(RULES["fixture_directories"])
FIXTURE_PATHS = tuple(tuple(parts) for parts in RULES["fixture_path_components"])


def is_source_code(path):
    return PurePosixPath(path).suffix.lower() in CODE_SUFFIXES


def excluded_material(path):
    p = PurePosixPath(path)
    return p.suffix.lower() in DATA_SUFFIXES or any(
        part.lower() in FIXTURE_DIRECTORIES for part in p.parts[:-1]
    ) or any(tuple(part.lower() for part in p.parts[i:i + len(parts)]) == parts
             for parts in FIXTURE_PATHS for i in range(len(p.parts) - len(parts)))


def notice_document(path):
    p = PurePosixPath(path)
    return not is_source_code(path) and not excluded_material(path) and (
        bool(NAME.match(p.name.lstrip(".")))
        or any(part.lower() in NOTICE_DIRECTORIES for part in p.parts[:-1])
    )


def notice_material(path, data):
    """One archive classification: legal document or explicitly typed excerpt."""
    if notice_document(path):
        return data, {}
    if (not excluded_material(path) and is_source_code(path)
            and NAME.match(PurePosixPath(path).name.lstrip("."))):
        header = legal_header(data)
        if header:
            import hashlib

            return header, {
                "material_type": "source-notice-excerpt",
                "source_file_sha256": hashlib.sha256(data).hexdigest(),
                "byte_range": [0, len(header)],
            }
    return None


def legal_header(data):
    """Return original [0,end) legal comment bytes, never executable code."""
    end = 0
    if data.startswith(b"/*"):
        close = data.find(b"*/")
        if close >= 0:
            end = close + 2
    elif data.startswith(b"//") or data.startswith(b"#"):
        prefix = b"//" if data.startswith(b"//") else b"#"
        for line in data.splitlines(keepends=True):
            if line.startswith(prefix) or not line.strip():
                end += len(line)
            else:
                break
    if 0 < end <= 64 * 1024:
        header = data[:end]
        if any(
            word in header.lower()
            for word in (b"copyright", b"license", b"licence", b"permission")
        ):
            return header
    return None
