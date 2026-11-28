import { describe, expect, it } from "vitest";
import { diffPair, hunkLabel, parseDiff } from "./doc-diff";

// Shaped like `git diff --relative=docs/` output from the doc store.
const single = [
  "diff --git a/notes/plan.md b/notes/plan.md",
  "index de98044..1ba297c 100644",
  "--- a/notes/plan.md",
  "+++ b/notes/plan.md",
  "@@ -1,3 +1,4 @@ heading",
  " a",
  "-b",
  "+B",
  " c",
  "+--- x",
  "\\ No newline at end of file",
  "",
].join("\n");

describe("parseDiff", () => {
  it("drops git headers and keeps hunks, lines and counts", () => {
    const [f, ...rest] = parseDiff(single, "notes/plan.md");
    expect(rest).toHaveLength(0);
    expect(f.path).toBe("notes/plan.md");
    expect([f.added, f.removed]).toEqual([2, 1]);
    expect(f.lines).toEqual([
      { kind: "hunk", text: "@@ -1,3 +1,4 @@" },
      { kind: "context", text: "a" },
      { kind: "remove", text: "b" },
      { kind: "add", text: "B" },
      { kind: "context", text: "c" },
      // A content line that looks like a header stays content.
      { kind: "add", text: "--- x" },
      { kind: "note", text: "No newline at end of file" },
    ]);
  });

  it("splits a folder's files, relative to the folder", () => {
    const folder = [
      "diff --git a/skills/x/SKILL.md b/skills/x/SKILL.md",
      "new file mode 100644",
      "--- /dev/null",
      "+++ b/skills/x/SKILL.md",
      "@@ -0,0 +1 @@",
      "+hi",
      "diff --git a/skills/x/ref.md b/skills/x/ref.md",
      "deleted file mode 100644",
      "--- a/skills/x/ref.md",
      "+++ /dev/null",
      "@@ -1 +0,0 @@",
      "-bye",
    ].join("\n");
    const files = parseDiff(folder, "skills/x");
    expect(files.map((f) => [f.path, f.added, f.removed])).toEqual([
      ["SKILL.md", 1, 0],
      ["ref.md", 0, 1],
    ]);
  });

  it("is empty for no changes", () => {
    expect(parseDiff("")).toEqual([]);
  });
});

describe("hunkLabel", () => {
  it("names the new side's lines", () => {
    expect(hunkLabel("@@ -1,3 +1,4 @@")).toBe("Lines 1–4");
    expect(hunkLabel("@@ -0,0 +1 @@")).toBe("Line 1");
    expect(hunkLabel("@@ -1 +0,0 @@")).toBe("Removed");
    expect(hunkLabel("@@ -4,2 +3,0 @@")).toBe("After line 3");
  });
});

describe("diffPair", () => {
  const v = ["c".repeat(40), "b".repeat(40), "a".repeat(40)];
  it("defaults to the previous version against the current one", () => {
    expect(diffPair(v)).toEqual({ from: v[1], to: v[0] });
  });
  it("compares a version with the one before it, or nothing for the first", () => {
    expect(diffPair(v, undefined, "bbbb")).toEqual({ from: v[2], to: v[1] });
    expect(diffPair(v, undefined, v[2])).toEqual({ from: null, to: v[2] });
  });
  it("keeps an explicit pair", () => {
    expect(diffPair(v, "aaaa", "cccc")).toEqual({ from: v[2], to: v[0] });
  });
});
