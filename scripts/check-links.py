#!/usr/bin/env python3
"""Check links. No dependencies.

  check-links.py SITE_DIR            every internal href/src in the built site
                                     resolves to a file, and every #anchor to an id
  check-links.py --markdown ROOT     every relative link in the repository's
                                     Markdown docs resolves to a tracked file
                                     (and a heading, for #anchors)
  --external                         also fetch http(s) links (HEAD, then GET)

Exits 1 and lists each broken link.
"""
from __future__ import annotations

import argparse
import html.parser
import pathlib
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from sitegen import slug  # noqa: E402

MARKDOWN_GLOBS = ["*.md", "docs/**/*.md", "kits/README.md", "tests/**/*.md", ".github/**/*.md"]


class Links(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.links: list[str] = []
        self.ids: set[str] = set()

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if name in ("href", "src") and value is not None:
                self.links.append(value)
            if name == "id" and value:
                self.ids.add(value)


def parse(path: pathlib.Path) -> Links:
    parser = Links()
    parser.feed(path.read_text(errors="replace"))
    return parser


def external_ok(url: str, cache: dict[str, bool]) -> bool:
    if url not in cache:
        ok = False
        for method in ("HEAD", "GET"):
            try:
                request = urllib.request.Request(url, method=method, headers={"User-Agent": "marsh-link-check"})
                with urllib.request.urlopen(request, timeout=15) as response:
                    ok = response.status < 400
            except (urllib.error.URLError, TimeoutError, ValueError):
                ok = False
            if ok:
                break
        cache[url] = ok
    return cache[url]


def check_site(root: pathlib.Path, external: bool) -> list[str]:
    root = root.resolve()
    pages = {p: parse(p) for p in root.rglob("*.html")}
    errors, cache = [], {}
    for page, parsed in pages.items():
        for link in parsed.links:
            where = f"{page.relative_to(root)}: {link}"
            if link.startswith(("http://", "https://")):
                if external and not external_ok(link.split("#")[0], cache):
                    errors.append(where + " (unreachable)")
                continue
            if re.match(r"^[a-z]+:", link):
                continue
            path, _, anchor = urllib.parse.unquote(link).partition("#")
            if not path:
                target = page
            elif path.startswith("/"):
                target = root / path.lstrip("/")
            else:
                target = (page.parent / path).resolve()
            if target.is_dir():
                target = target / "index.html"
            if not target.exists():
                errors.append(where + " (missing)")
                continue
            if anchor and target.suffix == ".html":
                ids = pages.get(target, parse(target)).ids
                if anchor not in ids:
                    errors.append(where + " (no such anchor)")
    errors += check_agent_files(root, pages)
    return errors


MD_LINK = re.compile(r"(?<!!)\[(?:[^\]\[]|\[[^\]]*\])*\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)")


def headings(path: pathlib.Path) -> set[str]:
    anchors, used, fence = set(), {}, False
    for line in path.read_text(errors="replace").splitlines():
        if line.lstrip().startswith(("```", "~~~")):
            fence = not fence
        if fence:
            continue
        m = re.match(r"^#{1,6}\s+(.*?)\s*#*\s*$", line)
        if m:
            text = re.sub(r"`([^`]*)`", r"\1", m.group(1))
            text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
            a = slug(text.replace("*", ""))
            if a in used:
                used[a] += 1
                a = f"{a}-{used[a]}"
            else:
                used[a] = 0
            anchors.add(a)
    return anchors


def check_agent_files(root: pathlib.Path, pages) -> list[str]:
    """Every link in the site's .md copies and llms*.txt that points into the
    site resolves to a built file (and, for .md targets, a heading)."""
    cname = root / "CNAME"
    host = f"https://{cname.read_text().strip()}/" if cname.exists() else None
    files = sorted(root.rglob("*.md")) + [p for p in (root / "llms.txt", root / "llms-full.txt") if p.exists()]
    errors = []
    for md in files:
        text = re.sub(r"```.*?```", "", md.read_text(errors="replace"), flags=re.S)
        text = re.sub(r"`[^`\n]*`", "", text)
        for link in MD_LINK.findall(text):
            where = f"{md.relative_to(root)}: {link}"
            if host and link.startswith(host):
                link = "/" + link[len(host):]
            elif link.startswith(("http://", "https://")) or re.match(r"^[a-z]+:", link):
                continue
            path, _, anchor = urllib.parse.unquote(link).partition("#")
            if not path:
                target = md
            elif path.startswith("/"):
                target = root / path.lstrip("/")
            else:
                target = (md.parent / path).resolve()
            if not target.exists():
                errors.append(where + " (missing)")
            elif anchor and target.suffix == ".md" and anchor not in headings(target):
                errors.append(where + " (no such heading)")
    return errors


def check_markdown(root: pathlib.Path, external: bool) -> list[str]:
    root = root.resolve()
    tracked = set(subprocess.run(["git", "ls-files", "--cached", "--others", "--exclude-standard"],
                                 cwd=root, capture_output=True, text=True, check=True).stdout.split())
    files = sorted({p for g in MARKDOWN_GLOBS for p in root.glob(g)
                    if p.relative_to(root).as_posix() in tracked and "vendor/" not in p.as_posix()})
    errors, cache = [], {}
    for md in files:
        text = md.read_text(errors="replace")
        text = re.sub(r"```.*?```", "", text, flags=re.S)
        text = re.sub(r"`[^`\n]*`", "", text)
        for link in MD_LINK.findall(text):
            where = f"{md.relative_to(root)}: {link}"
            if link.startswith(("http://", "https://")):
                if external and not external_ok(link.split("#")[0], cache):
                    errors.append(where + " (unreachable)")
                continue
            if re.match(r"^[a-z]+:", link):
                continue
            path, _, anchor = link.partition("#")
            target = (md.parent / urllib.parse.unquote(path)).resolve() if path else md
            try:
                rel = target.relative_to(root).as_posix()
            except ValueError:
                errors.append(where + " (outside the repository)")
                continue
            if not target.exists() or (target.is_file() and rel not in tracked) or \
                    (target.is_dir() and not any(t.startswith(rel + "/") for t in tracked)):
                errors.append(where + " (missing or untracked)")
                continue
            if anchor and target.suffix == ".md" and anchor not in headings(target):
                errors.append(where + " (no such heading)")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("path", type=pathlib.Path)
    parser.add_argument("--markdown", action="store_true")
    parser.add_argument("--external", action="store_true")
    args = parser.parse_args()
    errors = (check_markdown if args.markdown else check_site)(args.path, args.external)
    for error in errors:
        print(error, file=sys.stderr)
    print(f"check-links: {len(errors)} broken", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
