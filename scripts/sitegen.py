#!/usr/bin/env python3
"""Build the runmar.sh static site from the Markdown docs. No dependencies.

  sitegen.py --out DIR [--base-url https://runmar.sh]

Layout of DIR:
  index.html            landing page (docs/site/index.html)
  community.html        community page (docs/site/community.html)
  docs/index.html       docs/README.md
  docs/NAME.html        docs/NAME.md
  docs/design/...       docs/design/*.md, docs/architecture.md
  docs/marsh.1.html     docs/man/marsh.1.md
  docs/NAME.md          the same pages as Markdown for agents (links rewritten)
  llms.txt              index of the manual, one line per page (llmstxt.org format)
  llms-full.txt         the manual's pages concatenated in nav order
  install               scripts/install.sh (curl -fsSL https://runmar.sh/install | sh)
  style.css, site.js, favicon.svg, og.png, apple-touch-icon.png, CNAME,
  404.html, .nojekyll

Docs pages get the grouped manual nav, an on-page TOC (h2/h3), and prev/next
links in NAV order. og.png and apple-touch-icon.png are committed; rebuild them
from og.svg and favicon.svg with scripts/site-images.sh (headless Chrome).

Links to repository files that are not part of the site point at GitHub.
The Markdown converter handles the subset these docs use: ATX headings,
paragraphs, fenced and indented code, lists (nested), tables, block quotes,
rules, definition lists (TERM / ": text"), inline code, emphasis, links,
reference links, autolinks, and HTML comments (dropped).
"""
from __future__ import annotations

import argparse
import html
import pathlib
import posixpath
import re
import shutil

ROOT = pathlib.Path(__file__).resolve().parents[1]
SITE_SRC = ROOT / "docs/site"
GITHUB = "https://github.com/mcavage/marsh"


# --- Markdown -----------------------------------------------------------

def slug(text: str) -> str:
    """GitHub's heading anchor rule (close enough for these docs)."""
    text = re.sub(r"<[^>]+>", "", text).strip().lower()
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


class Inline:
    def __init__(self, refs: dict[str, str], link):
        self.refs = refs
        self.link = link

    def __call__(self, text: str) -> str:
        out: list[str] = []
        i = 0
        while i < len(text):
            c = text[i]
            if c == "`":
                n = len(text[i:]) - len(text[i:].lstrip("`"))
                end = text.find("`" * n, i + n)
                if end > 0:
                    code = text[i + n:end]
                    if code.startswith(" ") and code.endswith(" ") and code.strip():
                        code = code[1:-1]
                    out.append("<code>" + html.escape(code) + "</code>")
                    i = end + n
                    continue
            if c == "\\" and i + 1 < len(text) and text[i + 1] in "\\`*_[]()#+-.!|<>{}":
                out.append(html.escape(text[i + 1]))
                i += 2
                continue
            if c == "<":
                m = re.match(r"<(https?://[^>\s]+)>", text[i:])
                if m:
                    url = m.group(1)
                    out.append(f'<a href="{html.escape(url)}">{html.escape(url)}</a>')
                    i += m.end()
                    continue
                m = re.match(r"<!--.*?-->", text[i:])
                if m:
                    i += m.end()
                    continue
            if c == "[":
                m = self.match_link(text, i)
                if m:
                    label, target, end = m
                    out.append(f'<a href="{html.escape(self.link(target))}">{self(label)}</a>')
                    i = end
                    continue
            if text.startswith("**", i) or text.startswith("__", i):
                d = text[i:i + 2]
                end = text.find(d, i + 2)
                if end > i + 2:
                    out.append("<strong>" + self(text[i + 2:end]) + "</strong>")
                    i = end + 2
                    continue
            if c in "*_" and i + 1 < len(text) and not text[i + 1].isspace():
                prev = text[i - 1] if i else " "
                if c == "*" or not prev.isalnum():
                    end = i + 1
                    while True:
                        end = text.find(c, end)
                        if end < 0:
                            break
                        nxt = text[end + 1] if end + 1 < len(text) else " "
                        if not text[end - 1].isspace() and (c == "*" or not nxt.isalnum()) and \
                                not text.startswith(c * 2, end):
                            break
                        end += 1
                    if end > i + 1:
                        out.append("<em>" + self(text[i + 1:end]) + "</em>")
                        i = end + 1
                        continue
            out.append(html.escape(c))
            i += 1
        return "".join(out)

    def match_link(self, text: str, i: int):
        depth, j = 0, i
        while j < len(text):
            if text[j] == "\\":
                j += 2
                continue
            if text[j] == "`":
                k = text.find("`", j + 1)
                j = k + 1 if k > 0 else j + 1
                continue
            if text[j] == "[":
                depth += 1
            elif text[j] == "]":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        else:
            return None
        label = text[i + 1:j]
        rest = text[j + 1:]
        m = re.match(r"\(\s*<?([^)\s>]*)>?(?:\s+\"[^\"]*\")?\s*\)", rest)
        if m:
            return label, m.group(1), j + 1 + m.end()
        m = re.match(r"\[([^\]]*)\]", rest)
        if m:
            key = (m.group(1) or label).lower()
            if key in self.refs:
                return label, self.refs[key], j + 1 + m.end()
        if label.lower() in self.refs and not rest.startswith("("):
            return label, self.refs[label.lower()], j + 1
        return None


def markdown(text: str, link) -> tuple[str, str, list[tuple[int, str, str]]]:
    """Return (html, title, headings)."""
    lines = text.replace("\t", "    ").splitlines()
    refs: dict[str, str] = {}
    kept = []
    for line in lines:
        m = re.match(r"^ {0,3}\[([^\]]+)\]:\s*(\S+)", line)
        if m and not line.startswith("    "):
            refs[m.group(1).lower()] = m.group(2)
        else:
            kept.append(line)
    inline = Inline(refs, link)
    headings: list[tuple[int, str, str]] = []
    used: dict[str, int] = {}
    title = ""

    def heading(level: int, raw: str) -> str:
        nonlocal title
        rendered = inline(raw.strip().rstrip("#").strip())
        anchor = slug(rendered)
        if anchor in used:
            used[anchor] += 1
            anchor = f"{anchor}-{used[anchor]}"
        else:
            used[anchor] = 0
        if not title:
            title = re.sub(r"<[^>]+>", "", rendered)
        headings.append((level, anchor, rendered))
        return f'<h{level} id="{anchor}"><a class="anchor" href="#{anchor}" aria-hidden="true">#</a>{rendered}</h{level}>'

    def blocks(lines: list[str]) -> str:
        out: list[str] = []
        i = 0
        para: list[str] = []

        def flush():
            if para:
                out.append("<p>" + inline(" ".join(s.strip() for s in para)) + "</p>")
                para.clear()

        while i < len(lines):
            line = lines[i]
            stripped = line.strip()
            if not stripped:
                flush()
                i += 1
                continue
            if re.match(r"^<!--.*-->$", stripped):
                flush()
                i += 1
                continue
            m = re.match(r"^(#{1,6})\s+(.*)$", line)
            if m:
                flush()
                out.append(heading(len(m.group(1)), m.group(2)))
                i += 1
                continue
            m = re.match(r"^(\s*)(```+|~~~+)\s*([\w+-]*)", line)
            if m:
                flush()
                fence, lang, indent = m.group(2), m.group(3), len(m.group(1))
                i += 1
                code = []
                while i < len(lines) and not lines[i].strip().startswith(fence):
                    code.append(lines[i][indent:] if lines[i][:indent].strip() == "" else lines[i])
                    i += 1
                i += 1
                cls = f' class="language-{lang}"' if lang else ""
                out.append(f"<pre><code{cls}>" + html.escape("\n".join(code)) + "</code></pre>")
                continue
            if line.startswith("    ") and not para:
                code = []
                while i < len(lines) and (lines[i].startswith("    ") or not lines[i].strip()):
                    code.append(lines[i][4:])
                    i += 1
                while code and not code[-1].strip():
                    code.pop()
                out.append("<pre><code>" + html.escape("\n".join(code)) + "</code></pre>")
                continue
            if re.match(r"^ {0,3}([-*_])( *\1){2,} *$", line):
                flush()
                out.append("<hr>")
                i += 1
                continue
            if stripped.startswith(">"):
                flush()
                quote = []
                while i < len(lines) and lines[i].strip().startswith(">"):
                    quote.append(re.sub(r"^\s*> ?", "", lines[i]))
                    i += 1
                out.append("<blockquote>" + blocks(quote) + "</blockquote>")
                continue
            if stripped.startswith("|") and i + 1 < len(lines) and re.match(r"^\s*\|?\s*:?-{2,}", lines[i + 1]):
                flush()
                rows = []
                while i < len(lines) and lines[i].strip().startswith("|"):
                    rows.append(lines[i])
                    i += 1
                out.append(table(rows))
                continue
            m = re.match(r"^(\s*)([-*+]|\d+[.)])\s+", line)
            if m and (not para or len(m.group(1)) == 0):
                flush()
                i = lists(lines, i, out)
                continue
            if i + 1 < len(lines) and lines[i + 1].startswith(": ") and not para:
                out.append("<dl>")
                while i < len(lines) and i + 1 < len(lines) and lines[i + 1].startswith(": "):
                    out.append("<dt>" + inline(lines[i].strip()) + "</dt>")
                    i += 1
                    desc = [lines[i][2:]]
                    i += 1
                    while i < len(lines) and lines[i].startswith("  ") and lines[i].strip():
                        desc.append(lines[i].strip())
                        i += 1
                    out.append("<dd>" + inline(" ".join(desc)) + "</dd>")
                    while i < len(lines) and not lines[i].strip():
                        i += 1
                out.append("</dl>")
                continue
            para.append(line)
            i += 1
        flush()
        return "\n".join(out)

    def lists(lines: list[str], i: int, out: list[str]) -> int:
        m = re.match(r"^(\s*)([-*+]|\d+[.)])\s+", lines[i])
        base = len(m.group(1))
        ordered = m.group(2)[0].isdigit()
        tag = "ol" if ordered else "ul"
        start = int(re.match(r"\d+", m.group(2)).group()) if ordered else 1
        out.append(f'<{tag}{"" if start == 1 else f" start={start}"}>')
        items: list[list[str]] = []
        loose = False
        while i < len(lines):
            line = lines[i]
            m = re.match(r"^(\s*)([-*+]|\d+[.)])\s+(.*)$", line)
            if m and len(m.group(1)) == base and (m.group(2)[0].isdigit()) == ordered:
                items.append([m.group(3)])
                content_indent = len(line) - len(m.group(3))
                i += 1
                while i < len(lines):
                    nxt = lines[i]
                    if not nxt.strip():
                        if i + 1 < len(lines) and (lines[i + 1].startswith(" " * max(base + 2, 2))
                                                   and lines[i + 1].strip()):
                            items[-1].append("")
                            loose = loose or not re.match(r"^\s*([-*+]|\d+[.)])\s", lines[i + 1])
                            i += 1
                            continue
                        break
                    ind = len(nxt) - len(nxt.lstrip())
                    if ind <= base and re.match(r"^\s*([-*+]|\d+[.)])\s+", nxt):
                        break
                    if ind < content_indent and not items[-1][-1] == "" and ind <= base:
                        if re.match(r"^\s*(#|```|\|)", nxt):
                            break
                    items[-1].append(nxt[min(ind, content_indent):] if ind >= base + 2 else nxt.strip())
                    i += 1
                j = i
                while j < len(lines) and not lines[j].strip():
                    j += 1
                m2 = j < len(lines) and re.match(r"^(\s*)([-*+]|\d+[.)])\s+", lines[j])
                if m2 and len(m2.group(1)) == base:
                    if j > i:
                        loose = True
                    i = j
                    continue
                break
            else:
                break
        for item in items:
            body = blocks(item)
            if not loose and body.startswith("<p>"):
                body = re.sub(r"^<p>(.*?)</p>", r"\1", body, count=1, flags=re.S)
            out.append("<li>" + body + "</li>")
        out.append(f"</{tag}>")
        return i

    def table(rows: list[str]) -> str:
        def cells(row: str) -> list[str]:
            row = row.strip()
            row = row[1:] if row.startswith("|") else row
            row = row[:-1] if row.endswith("|") and not row.endswith("\\|") else row
            parts, cur, code = [], "", False
            k = 0
            while k < len(row):
                ch = row[k]
                if ch == "\\" and k + 1 < len(row) and row[k + 1] == "|":
                    cur += "|"
                    k += 2
                    continue
                if ch == "`":
                    code = not code
                if ch == "|" and not code:
                    parts.append(cur)
                    cur = ""
                else:
                    cur += ch
                k += 1
            parts.append(cur)
            return [p.strip() for p in parts]
        head = cells(rows[0])
        aligns = []
        for spec in cells(rows[1]):
            aligns.append("center" if spec.startswith(":") and spec.endswith(":") else
                          "right" if spec.endswith(":") else "")
        def td(tag, text, k):
            style = f' style="text-align:{aligns[k]}"' if k < len(aligns) and aligns[k] else ""
            return f"<{tag}{style}>{inline(text)}</{tag}>"
        body = ["<div class=\"table\"><table><thead><tr>" +
                "".join(td("th", h, k) for k, h in enumerate(head)) + "</tr></thead><tbody>"]
        for row in rows[2:]:
            body.append("<tr>" + "".join(td("td", c, k) for k, c in enumerate(cells(row))) + "</tr>")
        body.append("</tbody></table></div>")
        return "".join(body)

    # Drop YAML front matter (man page source).
    if kept and kept[0] == "---" and "---" in kept[1:]:
        kept = kept[kept.index("---", 1) + 1:]
    return blocks(kept), title, headings


# --- Site -----------------------------------------------------------------

def page_map() -> dict[pathlib.Path, str]:
    """Source file -> site path."""
    pages: dict[pathlib.Path, str] = {}
    for source in sorted((ROOT / "docs").glob("*.md")):
        name = "index" if source.name == "README.md" else source.stem
        pages[source] = f"docs/{name}.html"
    for source in sorted((ROOT / "docs/design").glob("*.md")):
        name = "index" if source.name == "README.md" else source.stem
        pages[source] = f"docs/design/{name}.html"
    pages[ROOT / "docs/man/marsh.1.md"] = "docs/marsh.1.html"
    return pages


NAV_GROUPS = [
    ("Start", [("Overview", "docs/index.html"), ("Install", "docs/install.html"),
               ("Quickstart", "docs/quickstart.html")]),
    ("Use", [("The shell", "docs/shell.html"), ("Choosing a shell", "docs/shells.html"),
             ("Commands and Kits", "docs/kits.html")]),
    ("Compose", [("Agents running agents", "docs/agents.html"), ("Split and join", "docs/split.html"),
                 ("Fanout and collect", "docs/fanout.html")]),
    ("Connect", [("Agent sessions (ACP)", "docs/acp.html"), ("MCP", "docs/mcp.html")]),
    ("Reference", [("Configuration", "docs/configuration.html"),
                   ("Troubleshooting", "docs/troubleshooting.html"), ("Security", "docs/security.html"),
                   ("FAQ", "docs/faq.html"), ("marsh(1)", "docs/marsh.1.html"),
                   ("Design", "docs/design/index.html")]),
]
NAV = [item for _, items in NAV_GROUPS for item in items]

# One line per manual page, for <meta name=description> and llms.txt. Pages not
# listed here fall back to their first paragraph.
DESCRIPTIONS = {
    "docs/index.html": "Manual index: what marsh is and where each topic lives.",
    "docs/install.html": "Install marsh and Docker Sandboxes with Homebrew or the install script. Upgrade, uninstall, build from source.",
    "docs/quickstart.html": "Open a shell, run an agent, split work across two agents, and read the results, in about ten minutes.",
    "docs/shell.html": "The Bash-compatible shell: where it runs, startup files, and the documented differences from Bash.",
    "docs/shells.html": "Use the shell VM's own bash or zsh instead of Brush, and what you lose.",
    "docs/kits.html": "Registered commands and Kits: the default claude, codex, and pi commands, signing in, adding your own, and limits.",
    "docs/agents.html": "Agents that call other agents: child jobs, limits, MARSH_SPAWN, Ctrl-C, and what each job can see.",
    "docs/split.html": "split and join: run commands in parallel on private copies of your project and get patches back.",
    "docs/fanout.html": "fanout and collect: run commands side by side on the same files and gather their output.",
    "docs/acp.html": "ACP agent sessions: keep one agent running across prompts, and publish a session as a tool.",
    "docs/mcp.html": "MCP: publish shell pipelines as tools for agents, and let Claude Code or Codex drive marsh.",
    "docs/configuration.html": "Environment variables, files, job limits, and where marsh keeps its state.",
    "docs/troubleshooting.html": "Symptoms and fixes: slow first call, sbx errors, stuck jobs, and how to reset or stop VMs.",
    "docs/security.html": "What marsh isolates, what it does not, and what enforces each boundary.",
    "docs/faq.html": "Common questions: what marsh is, how it works, what it costs, and what can go wrong.",
    "docs/marsh.1.html": "The marsh(1) manual page: every command, option, and exit status.",
    "docs/design/index.html": "Design documents: architecture, contracts, and the TLA+ model.",
}


def strip_tags(text: str) -> str:
    return html.unescape(re.sub(r"<[^>]+>", "", text))


def describe(body: str, title: str) -> str:
    """The first paragraph, as plain text, for <meta name=description>."""
    m = re.search(r"<p>(.*?)</p>", body, re.S)
    text = " ".join(strip_tags(m.group(1)).split()) if m else ""
    if len(text) > 160:
        text = text[:157].rsplit(" ", 1)[0].rstrip(",;:") + "…"
    return text or f"{title}: marsh documentation."


def toc_html(headings: list[tuple[int, str, str]]) -> str:
    items = []
    # The man page uses only top-level sections (NAME, SYNOPSIS, ...).
    levels = (2, 3) if any(level == 2 for level, _, _ in headings) else (1,)
    for level, anchor, rendered in headings:
        if level in levels:
            label = re.sub(r'<a class="anchor"[^>]*>#</a>', "", rendered)
            label = re.sub(r"</?a\b[^>]*>", "", label)
            items.append(f'<li class="l{max(level, 2)}"><a href="#{anchor}">{label}</a></li>')
    if len(items) < 2:
        return ""
    return ('  <nav class="toc" aria-labelledby="toc-h">\n    <h2 id="toc-h">On this page</h2>\n'
            '    <ul>\n' + "\n".join("      " + i for i in items) + "\n    </ul>\n  </nav>")


def nav_html(site_path: str) -> str:
    out = []
    for group, items in NAV_GROUPS:
        out.append(f"      <h2>{html.escape(group)}</h2>\n      <ul>")
        for label, target in items:
            current = ' aria-current="page"' if target == site_path else ""
            out.append(f'        <li><a href="{relative(site_path, target)}"{current}>{html.escape(label)}</a></li>')
        out.append("      </ul>")
    return "\n".join(out)


def pager_html(site_path: str) -> str:
    order = [target for _, target in NAV]
    if site_path not in order:
        return ""
    k = order.index(site_path)
    links = []
    if k > 0:
        label, target = NAV[k - 1]
        links.append(f'<a class="prev" href="{relative(site_path, target)}" rel="prev">'
                     f'<span>Previous</span>{html.escape(label)}</a>')
    if k + 1 < len(NAV):
        label, target = NAV[k + 1]
        links.append(f'<a class="next" href="{relative(site_path, target)}" rel="next">'
                     f'<span>Next</span>{html.escape(label)}</a>')
    return '    <nav class="pager" aria-label="Previous and next">' + "".join(links) + "</nav>"


def relative(from_page: str, to_page: str) -> str:
    rel = posixpath.relpath(to_page, posixpath.dirname(from_page) or ".")
    return rel


MD_INLINE = re.compile(r"(?<!!)(\[(?:[^\]\[]|\[[^\]]*\])*\]\()(\s*<?)([^)\s>]+)(>?(?:\s+\"[^\"]*\")?\s*\))")
MD_REF = re.compile(r"^(\s{0,3}\[[^\]]+\]:\s*)(\S+)")


def md_path(site_path: str) -> str:
    return site_path[: -len(".html")] + ".md"


def agent_markdown(text: str, source: pathlib.Path, site_path: str,
                   by_source: dict[pathlib.Path, str], base_url: str) -> str:
    """The page's Markdown for agents: front matter and HTML comments dropped, and
    relative links pointing at other site pages' .md copies (absolute URLs, so the
    text still works when pasted or concatenated). Other repository links go to GitHub."""
    lines = text.split("\n")
    if lines and lines[0] == "---" and "---" in lines[1:]:
        lines = lines[lines.index("---", 1) + 1:]

    def rewrite(target: str) -> str:
        if re.match(r"^[a-z]+:", target) or target.startswith(("#", "//")):
            return target
        path, _, anchor = target.partition("#")
        if not path:
            return target
        resolved = (source.parent / path).resolve()
        suffix = "#" + anchor if anchor else ""
        if resolved in by_source:
            return f"{base_url}/{md_path(by_source[resolved])}{suffix}"
        try:
            repo_path = resolved.relative_to(ROOT).as_posix()
        except ValueError:
            return target
        kind = "tree" if resolved.is_dir() else "blob"
        return f"{GITHUB}/{kind}/main/{repo_path}{suffix}"

    # Fenced code stays as written. In the rest, mask inline code (a link's text
    # may contain it), rewrite links (their text may span lines), then restore.
    blocks: list[tuple[bool, list[str]]] = []
    fence = False
    for line in lines:
        starts = line.lstrip().startswith(("```", "~~~"))
        if starts and not fence:
            blocks.append((True, [line]))
            fence = True
        elif fence:
            blocks[-1][1].append(line)
            if starts:
                fence = False
        else:
            if not blocks or blocks[-1][0]:
                blocks.append((False, []))
            blocks[-1][1].append(line)
    out: list[str] = []
    for is_code, block in blocks:
        if is_code:
            out.extend(block)
            continue
        block = [line for line in block if not re.match(r"^\s*<!--.*-->\s*$", line)]
        spans: list[str] = []

        def mask(m: re.Match) -> str:
            spans.append(m.group(0))
            return f"\x00{len(spans) - 1}\x00"

        text = re.sub(r"`[^`\n]*`", mask, "\n".join(block))
        text = MD_INLINE.sub(lambda m: m.group(1) + m.group(2) + rewrite(m.group(3)) + m.group(4), text)
        text = "\n".join(MD_REF.sub(lambda m: m.group(1) + rewrite(m.group(2)), line) for line in text.split("\n"))
        text = re.sub(r"\x00(\d+)\x00", lambda m: spans[int(m.group(1))], text)
        out.extend(text.split("\n"))
    return "\n".join(out).strip("\n") + "\n"


LLMS_SUMMARY = (
    "marsh is a Bash-compatible shell for Apple Silicon Macs. It adds `split`, `fanout`, `join`, "
    "and `collect` for running agents in parallel, each on its own copy of your repo, and runs "
    "every agent and tool in its own Docker Sandbox microVM."
)


def llms_txt(base_url: str, entries: dict[str, tuple[str, str]]) -> str:
    """entries: site path -> (title, description)."""
    lines = ["# marsh", "", "> " + LLMS_SUMMARY, "",
             f"Every manual page is also available as Markdown at the same path with a .md "
             f"extension. The whole manual is in one file: [llms-full.txt]({base_url}/llms-full.txt).", ""]
    listed: set[str] = set()
    for group, items in NAV_GROUPS:
        lines.append(f"## {group}")
        lines.append("")
        for label, target in items:
            if target in entries:
                lines.append(f"- [{label}]({base_url}/{md_path(target)}): {entries[target][1]}")
                listed.add(target)
        lines.append("")
    rest = sorted(t for t in entries if t not in listed)
    if rest:
        lines.append("## Optional")
        lines.append("")
        for target in rest:
            title, description = entries[target]
            lines.append(f"- [{title}]({base_url}/{md_path(target)}): {description}")
        lines.append("")
    return "\n".join(lines)


def build(out: pathlib.Path, base_url: str) -> None:
    pages = page_map()
    by_source = {p.resolve(): site for p, site in pages.items()}
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    template = (SITE_SRC / "page.html").read_text()
    entries: dict[str, tuple[str, str]] = {}
    markdown_copies: dict[str, str] = {}

    for source, site_path in pages.items():
        def link(target: str, source=source, site_path=site_path) -> str:
            if re.match(r"^[a-z]+:", target) or target.startswith("#") or target.startswith("//"):
                return target
            path, _, anchor = target.partition("#")
            if not path:
                return target
            resolved = (source.parent / path).resolve()
            if resolved in by_source:
                dest = relative(site_path, by_source[resolved])
                return dest + ("#" + anchor if anchor else "")
            try:
                repo_path = resolved.relative_to(ROOT).as_posix()
            except ValueError:
                return target
            kind = "tree" if resolved.is_dir() else "blob"
            return f"{GITHUB}/{kind}/main/{repo_path}" + ("#" + anchor if anchor else "")

        body, title, headings = markdown(source.read_text(), link)
        repo_path = source.relative_to(ROOT).as_posix()
        title = title or "marsh"
        if source.parent.name == "man":
            title = f"{source.stem.rsplit('.', 1)[0]}({source.suffixes[0][1:]})"
        fields = {
            "title": html.escape(title),
            "description": html.escape(DESCRIPTIONS.get(site_path) or describe(body, title)),
            "root": relative(site_path, "index.html").removesuffix("index.html") or "./",
            "nav": nav_html(site_path),
            "toc": toc_html(headings),
            "pager": pager_html(site_path),
            "source": f"{GITHUB}/blob/main/{repo_path}",
            "canonical": f"{base_url}/{site_path}",
            "base": base_url,
        }
        fields["md"] = posixpath.basename(md_path(site_path))
        entries[site_path] = (title, DESCRIPTIONS.get(site_path) or describe(body, title))
        agent_md = agent_markdown(source.read_text(), source, site_path, by_source, base_url)
        markdown_copies[site_path] = agent_md
        md_target = out / md_path(site_path)
        md_target.parent.mkdir(parents=True, exist_ok=True)
        md_target.write_text(agent_md)
        # {{content}} last, so text in the docs is never read as a placeholder.
        page = template
        for key, value in fields.items():
            page = page.replace("{{" + key + "}}", value)
        page = page.replace("{{content}}", body)
        target = out / site_path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(page)

    landing = (SITE_SRC / "index.html").read_text()
    landing = landing.replace("{{canonical}}", base_url + "/").replace("{{base}}", base_url)
    (out / "index.html").write_text(landing)
    community = (SITE_SRC / "community.html").read_text()
    community = community.replace("{{canonical}}", base_url + "/community.html").replace("{{base}}", base_url)
    (out / "community.html").write_text(community)
    for name in ("style.css", "site.js", "404.html", "favicon.svg", "og.png", "apple-touch-icon.png"):
        shutil.copy(SITE_SRC / name, out / name)
    (out / "llms.txt").write_text(llms_txt(base_url, entries))
    full = []
    for _, target in NAV:
        if target in markdown_copies and not target.startswith("docs/design/"):
            full.append(f"<!-- Source: {base_url}/{md_path(target)} -->\n\n{markdown_copies[target]}")
    (out / "llms-full.txt").write_text("# marsh manual\n\n> " + LLMS_SUMMARY + "\n\n---\n\n" +
                                      "\n---\n\n".join(full))
    shutil.copy(ROOT / "scripts/install.sh", out / "install")
    (out / "CNAME").write_text(base_url.split("://", 1)[1].rstrip("/") + "\n")
    (out / ".nojekyll").write_text("")
    print(f"site: {len(pages) + 2} pages in {out}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.add_argument("--base-url", default="https://runmar.sh")
    args = parser.parse_args()
    build(args.out, args.base_url.rstrip("/"))


if __name__ == "__main__":
    main()
