import { describe, expect, test } from "vitest";
import { docScopes, formatBytes, matches, previewKind, storeLink } from "./store";
import { artifactReference } from "./artifact-reference";

const sha = "a".repeat(64);
describe("store links", () => {
  test("signed links parse on any host", () => {
    const l = storeLink(`http://axe.hlab/b/${sha}/shot%20one.png?exp=1795655430&sig=ff`);
    expect(l).toEqual({
      sha256: sha,
      name: "shot one.png",
      path: `/b/${sha}/shot%20one.png?exp=1795655430&sig=ff`,
      expires: 1795655430000,
    });
    expect(storeLink(`https://files.example/b/${sha}?exp=1&sig=x`)?.name).toBe("");
  });
  test("unsigned or malformed paths are not store links", () => {
    expect(storeLink(`http://axe.hlab/b/${sha}/x.png`)).toBeUndefined();
    expect(storeLink(`http://axe.hlab/b/NOTHEX/x.png?exp=1&sig=x`)).toBeUndefined();
    expect(storeLink(`http://axe.hlab/b/${sha}/a/b?exp=1&sig=x`)).toBeUndefined();
    expect(storeLink("not a url")).toBeUndefined();
  });
  test("artifact references name store links", () => {
    const r = artifactReference(`http://axe.hlab/b/${sha}/map.tsv?exp=1&sig=x`);
    expect(r.type).toBe("Store");
    expect(r.name).toBe("map.tsv");
    expect(r.blob?.sha256).toBe(sha);
    expect(artifactReference(`http://axe.hlab/b/${sha}?exp=1&sig=x`).name).toBe(
      `blob ${sha.slice(0, 12)}`,
    );
  });
});

test("preview kinds follow the content type, then the extension", () => {
  expect(previewKind("image/png")).toBe("image");
  expect(previewKind("text/html; charset=utf-8")).toBe("html");
  expect(previewKind("text/markdown; charset=utf-8")).toBe("markdown");
  expect(previewKind("text/plain", "notes.md")).toBe("markdown");
  expect(previewKind("application/json")).toBe("text");
  expect(previewKind("application/octet-stream", "run.log")).toBe("text");
  expect(previewKind("application/octet-stream", "shot.PNG")).toBe("image");
  expect(previewKind("", "page.html")).toBe("html");
  expect(previewKind("application/pdf", "r.pdf")).toBe("download");
  expect(previewKind("application/octet-stream")).toBe("download");
});

test("sizes, search and doc labels", () => {
  expect(formatBytes(37)).toBe("37 B");
  expect(formatBytes(329023)).toBe("321 KiB");
  expect(formatBytes(1.5 * 1024 * 1024)).toBe("1.5 MiB");
  expect(matches(["dufs-artifact-map.tsv", "#1241"], "map #1241")).toBe(true);
  expect(matches(["a.png"], "map")).toBe(false);
  const att = [
    { scope: "label:harvis", path: "harvis/docs/x.md", exists: true },
    { scope: "global", path: "skills/axe", exists: true },
    { scope: "label:ops", path: "skills/axe/", exists: true },
  ];
  expect(docScopes("skills/axe/SKILL.md", att)).toEqual(["global", "label:ops"]);
  expect(docScopes("harvis/docs/x.md", att)).toEqual(["label:harvis"]);
  expect(docScopes("skills/axel/SKILL.md", att)).toEqual([]);
});
