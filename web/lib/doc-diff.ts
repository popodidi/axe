// A doc's history on the doc page (components/dashboard/doc-history.tsx):
// the git unified diff `/api/docs/diff` returns (crates/axe/src/doc.rs
// `Store::diff`), split into files and lines, and the default pair of
// versions to compare.

export type DiffLine = {
  kind: "add" | "remove" | "context" | "hunk" | "note";
  text: string;
};
export type DiffFile = { path: string; lines: DiffLine[]; added: number; removed: number };

// Files in diff order. Paths are relative to the store's docs/ (`a/<path>`),
// so `docPath/` is cut for a folder's files. Git's header lines (index,
// modes, ---/+++) are dropped; a hunk keeps its `@@` line.
export function parseDiff(diff: string, docPath = ""): DiffFile[] {
  const files: DiffFile[] = [];
  let file: DiffFile | null = null;
  let inHunk = false;
  const lines = diff.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  for (const line of lines) {
    const head = line.match(/^diff --git a\/(.*) b\/(.*)$/);
    if (head) {
      const p = head[2];
      const rel = docPath && p.startsWith(`${docPath}/`) ? p.slice(docPath.length + 1) : p;
      file = { path: rel, lines: [], added: 0, removed: 0 };
      files.push(file);
      inHunk = false;
      continue;
    }
    if (!file) continue;
    if (line.startsWith("@@")) {
      inHunk = true;
      file.lines.push({ kind: "hunk", text: line.replace(/^(@@[^@]*@@).*$/, "$1") });
      continue;
    }
    if (!inHunk) continue;
    if (line.startsWith("+")) {
      file.added++;
      file.lines.push({ kind: "add", text: line.slice(1) });
    } else if (line.startsWith("-")) {
      file.removed++;
      file.lines.push({ kind: "remove", text: line.slice(1) });
    } else if (line.startsWith("\\")) {
      file.lines.push({ kind: "note", text: line.slice(2) });
    } else {
      file.lines.push({ kind: "context", text: line.slice(1) });
    }
  }
  return files;
}

// A hunk's `@@ -12,3 +12,5 @@` as the new side's line range.
export function hunkLabel(text: string): string {
  const m = text.match(/\+(\d+)(?:,(\d+))?/);
  if (!m) return text;
  const start = Number(m[1]);
  const count = m[2] === undefined ? 1 : Number(m[2]);
  if (count === 0) return start ? `After line ${start}` : "Removed";
  return count === 1 ? `Line ${start}` : `Lines ${start}–${start + count - 1}`;
}

// The versions to compare, given the URL's (possibly short or missing)
// `from` and `to` and the doc's versions newest first: `to` defaults to the
// current version, `from` to the one before `to` (null: `to` is the first,
// compared with nothing).
export function diffPair(
  versions: string[],
  from?: string,
  to?: string,
): { from: string | null; to: string } {
  const find = (v: string) => versions.find((x) => x.startsWith(v)) || v;
  const t = to ? find(to) : versions[0] || "";
  if (from) return { from: find(from), to: t };
  const i = versions.indexOf(t);
  return { from: i >= 0 ? versions[i + 1] || null : null, to: t };
}
