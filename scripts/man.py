#!/usr/bin/env python3
"""Render docs/man/*.md to roff man pages (and check them with mandoc).

The source is a small Markdown subset, so one file feeds both the man page and
the documentation site:

  ---  front matter (title, section, date, source, manual)  ---
  # SECTION            .SH
  **bold**, *italic*   \\fB \\fI
  TERM                 a definition: the term line, then lines starting
  : description          with ": " (continued by two-space indented lines)
      code             four-space indented block, printed verbatim

Usage: man.py OUTDIR [SOURCE.md...]   (default sources: docs/man/*.md)
"""
from __future__ import annotations

import pathlib
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]


def escape(text: str) -> str:
    text = text.replace("\\", "\\e")
    text = text.replace("-", "\\-")
    text = re.sub(r"\*\*(.+?)\*\*", r"\\fB\1\\fR", text)
    text = re.sub(r"(?<![\w*])\*(?!\s)(.+?)(?<!\s)\*(?![\w*])", r"\\fI\1\\fR", text)
    text = re.sub(r"`([^`]+)`", r"\\fB\1\\fR", text)
    return text


def line(text: str) -> str:
    """A roff text line; a leading . or ' would be read as a request."""
    text = escape(text)
    return "\\&" + text if text.startswith((".", "'")) else text


def render(source: pathlib.Path) -> tuple[str, str]:
    lines = source.read_text().splitlines()
    meta: dict[str, str] = {}
    if lines and lines[0] == "---":
        end = lines.index("---", 1)
        for entry in lines[1:end]:
            key, _, value = entry.partition(":")
            meta[key.strip()] = value.strip()
        lines = lines[end + 1:]
    title, section = meta["title"], meta["section"]
    out = [f'.TH {title} {section} "{meta.get("date", "")}" "{meta.get("source", "")}" "{meta.get("manual", "")}"']
    i = 0
    paragraph: list[str] = []

    def flush() -> None:
        if paragraph:
            if not out[-1].startswith(".SH"):
                out.append(".PP")
            out.extend(line(text) for text in paragraph)
            paragraph.clear()

    while i < len(lines):
        text = lines[i]
        if text.startswith("# "):
            flush()
            out.append(".SH " + text[2:].strip())
        elif not text.strip():
            flush()
        elif text.startswith("    "):
            flush()
            out.extend(([] if out[-1].startswith(".SH") else [".PP"]) + [".RS 4", ".nf"])
            while i < len(lines) and (lines[i].startswith("    ") or not lines[i].strip()):
                if lines[i].strip() or (i + 1 < len(lines) and lines[i + 1].startswith("    ")):
                    out.append(line(lines[i][4:]))
                i += 1
            out.extend([".fi", ".RE"])
            continue
        elif i + 1 < len(lines) and lines[i + 1].startswith(": "):
            flush()
            out.extend([".TP", line(text)])
            i += 1
            out.append(line(lines[i][2:]))
            while i + 1 < len(lines) and lines[i + 1].startswith("  ") and lines[i + 1].strip():
                i += 1
                out.append(line(lines[i].strip()))
        else:
            paragraph.append(text.strip())
        i += 1
    flush()
    name = source.name.removesuffix(".md")
    return name, "\n".join(out) + "\n"


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    outdir = pathlib.Path(sys.argv[1])
    sources = [pathlib.Path(p) for p in sys.argv[2:]] or sorted((ROOT / "docs/man").glob("*.md"))
    outdir.mkdir(parents=True, exist_ok=True)
    status = 0
    for source in sources:
        name, roff = render(source)
        target = outdir / name
        target.write_text(roff)
        print(target)
        if shutil.which("mandoc"):
            lint = subprocess.run(["mandoc", "-T", "lint", "-W", "warning", str(target)],
                                  capture_output=True, text=True)
            report = (lint.stdout + lint.stderr).strip()
            if report:
                print(report, file=sys.stderr)
                status = 1
    # msh is a link to marsh; so is its page.
    if (outdir / "marsh.1").exists():
        (outdir / "msh.1").write_text(".so man1/marsh.1\n")
        print(outdir / "msh.1")
    return status


if __name__ == "__main__":
    sys.exit(main())
