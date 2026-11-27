#!/usr/bin/env python3
"""Check the relative links in README.md and docs/user/ (no dependencies).

Every Markdown link or image whose target is not a URL must name a file or
directory in the repository, and a `#fragment` on a Markdown target must match
one of its headings (GitHub's anchor rules). Under docs/user/, every page also
needs YAML front matter with `title` and `description`, and every `meta.json`
`pages` entry must name a page or folder: the docs site reads both.

Usage: python3 scripts/check_doc_links.py [FILE_OR_DIR ...]
(default README.md docs/user). Exits non-zero listing each broken link.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs" / "user"

FENCE = re.compile(r"^\s*(```|~~~)")
CODE_SPAN = re.compile(r"(`+)(?:(?!\1).)+?\1")
# [text](target) or ![alt](target "title"); target may be wrapped in <>.
LINK = re.compile(r"!?\[(?:[^\]\\]|\\.)*\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)")
HEADING = re.compile(r"^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$")
URL = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")


def prose_lines(text):
    """Yield (line number, line) outside fenced code, with code spans blanked."""
    fence = None
    for n, line in enumerate(text.split("\n"), 1):
        m = FENCE.match(line)
        if m:
            if fence is None:
                fence = m.group(1)
            elif m.group(1) == fence:
                fence = None
            continue
        if fence is None:
            yield n, CODE_SPAN.sub(lambda c: " " * len(c.group(0)), line)


def slug(heading):
    """GitHub's anchor for a heading's text."""
    text = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", heading)  # links keep their text
    text = re.sub(r"<[^>]+>", "", text).replace("`", "")
    text = re.sub(r"[^\w\- ]", "", text.lower())
    return text.replace(" ", "-")


_anchors = {}


def anchors(path):
    if path not in _anchors:
        seen, out = {}, set()
        for _, line in prose_lines(path.read_text(encoding="utf-8")):
            m = HEADING.match(line)
            if m:
                base = slug(m.group(2))
                k = seen.get(base, 0)
                seen[base] = k + 1
                out.add(base if k == 0 else f"{base}-{k}")
        _anchors[path] = out
    return _anchors[path]


def check_links(path, errors):
    rel = path.relative_to(ROOT)
    for n, line in prose_lines(path.read_text(encoding="utf-8")):
        for m in LINK.finditer(line):
            target = m.group(1)
            if URL.match(target):
                continue
            file_part, _, frag = target.partition("#")
            dest = (path.parent / file_part).resolve() if file_part else path
            if not dest.exists():
                errors.append(f"{rel}:{n}: {target}: no such file")
                continue
            if frag and dest.suffix == ".md" and frag not in anchors(dest):
                errors.append(f"{rel}:{n}: {target}: no heading #{frag} in {dest.relative_to(ROOT)}")


def check_front_matter(path, errors):
    rel = path.relative_to(ROOT)
    lines = path.read_text(encoding="utf-8").split("\n")
    if not lines or lines[0] != "---" or "---" not in lines[1:]:
        errors.append(f"{rel}:1: missing YAML front matter")
        return
    keys = {l.split(":", 1)[0].strip() for l in lines[1 : lines.index("---", 1)] if ":" in l}
    for key in ("title", "description"):
        if key not in keys:
            errors.append(f"{rel}:1: front matter has no {key}")


def check_meta(path, errors):
    rel = path.relative_to(ROOT)
    try:
        meta = json.loads(path.read_text(encoding="utf-8"))
    except ValueError as e:
        errors.append(f"{rel}: invalid JSON: {e}")
        return
    if not isinstance(meta.get("title"), str) or not isinstance(meta.get("pages"), list):
        errors.append(f'{rel}: needs {{"title": "...", "pages": [...]}}')
        return
    for page in meta["pages"]:
        if not (path.parent / f"{page}.md").is_file() and not (path.parent / page).is_dir():
            errors.append(f"{rel}: page {page!r} is neither {page}.md nor a folder")


def main(args):
    targets = [Path(a).resolve() for a in args] or [ROOT / "README.md", DOCS]
    errors, files = [], 0
    for t in targets:
        if not t.exists():
            errors.append(f"{t}: no such file or directory")
            continue
        for f in sorted(t.rglob("*")) if t.is_dir() else [t]:
            in_docs = DOCS == f or DOCS in f.parents
            if f.suffix == ".md":
                files += 1
                check_links(f, errors)
                if in_docs:
                    check_front_matter(f, errors)
            elif f.name == "meta.json" and in_docs:
                check_meta(f, errors)
    for e in errors:
        print(e)
    print(f"checked {files} Markdown files: {len(errors)} problem(s)")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
