import { describe, expect, it } from "vitest";
import {
  codeLanguage,
  docKind,
  highlightLines,
  imageSrc,
  isBinary,
  parseDelimited,
  prettyJson,
} from "./doc-render";

const PNG =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

describe("doc kinds", () => {
  it("detects by extension", () => {
    expect(docKind("a/b.md", "# x")).toBe("markdown");
    expect(docKind("r.html", "<p>x</p>")).toBe("html");
    for (const p of [
      "c.json",
      "c.yaml",
      "c.toml",
      "s.sh",
      "m.py",
      "l.rs",
      "t.ts",
      "g.go",
      "q.sql",
      "s.css",
      "Makefile",
    ])
      expect(docKind(p, "x"), p).toBe("code");
    expect(docKind("d.csv", "a,b")).toBe("table");
    expect(docKind("d.tsv", "a\tb")).toBe("table");
    expect(docKind("i.png", PNG)).toBe("image");
    expect(docKind("i.svg", "<svg xmlns='http://www.w3.org/2000/svg'/>")).toBe("image");
    expect(docKind("notes.weird", "plain")).toBe("text");
    expect(docKind("noext", "plain")).toBe("text");
  });

  it("prefers the store's content type", () => {
    expect(docKind("x.txt", "# x", "text/markdown; charset=utf-8")).toBe("markdown");
    expect(docKind("x.md", "<p/>", "text/html")).toBe("html");
    expect(docKind("x", "{}", "application/json")).toBe("code");
    expect(docKind("x", "a,b", "text/csv")).toBe("table");
    expect(docKind("x", PNG, "image/png")).toBe("image");
  });

  it("shows binary content as binary, whatever its name", () => {
    expect(isBinary("ab\0cd")).toBe(true);
    expect(isBinary("��PNG�")).toBe(true);
    expect(isBinary("plain café")).toBe(false);
    expect(docKind("x.json", "\0\0")).toBe("binary");
    expect(docKind("x.png", "�PNG��\0")).toBe("binary");
    // An image name over text that is not an image is plain text.
    expect(docKind("x.png", "not an image")).toBe("text");
  });

  it("maps languages", () => {
    expect(codeLanguage("a.yml")).toBe("yaml");
    expect(codeLanguage("a.toml")).toBe("ini");
    expect(codeLanguage("a.tsx")).toBe("typescript");
    expect(codeLanguage("a.unknown")).toBe("");
  });
});

describe("images", () => {
  it("builds data URLs from base64, data URLs and SVG text", () => {
    expect(imageSrc("a.png", PNG)).toBe(`data:image/png;base64,${PNG}`);
    expect(imageSrc("a.jpg", `data:image/jpeg;base64,${PNG}`)).toBe(
      `data:image/jpeg;base64,${PNG}`,
    );
    expect(imageSrc("a.svg", "<svg><script>x</script></svg>")).toMatch(
      /^data:image\/svg\+xml;charset=utf-8,%3Csvg/,
    );
    expect(imageSrc("a.txt", PNG)).toBeNull();
    expect(imageSrc("a.png", "hello world")).toBeNull();
  });
});

describe("parsing", () => {
  it("pretty-prints JSON and leaves bad JSON alone", () => {
    expect(prettyJson('{"a":[1,2]}')).toBe('{\n  "a": [\n    1,\n    2\n  ]\n}');
    expect(prettyJson("{oops")).toBe("{oops");
  });

  it("parses CSV with quotes and TSV", () => {
    expect(parseDelimited('a,b\n"x, y","say ""hi"""\r\n"multi\nline",2\n', ",")).toEqual([
      ["a", "b"],
      ["x, y", 'say "hi"'],
      ["multi\nline", "2"],
    ]);
    expect(parseDelimited("url\tsha\nu\ts", "\t")).toEqual([
      ["url", "sha"],
      ["u", "s"],
    ]);
  });

  it("highlights into lines, multi-line tokens split per line", () => {
    const lines = highlightLines("/* a\nb */\nlet x = 1;\n", "javascript");
    expect(lines).toHaveLength(3);
    expect(lines[0][0]).toEqual({ text: "/* a", className: "hljs-comment" });
    expect(lines[1][0]).toEqual({ text: "b */", className: "hljs-comment" });
    expect(lines[2].some((t) => t.className === "hljs-keyword" && t.text === "let")).toBe(true);
    expect(highlightLines("a\nb", "")).toEqual([[{ text: "a" }], [{ text: "b" }]]);
  });
});
