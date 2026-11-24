// How the doc page (components/dashboard/doc-file.tsx) shows one stored
// file: by the store's content type when it gives one, else by extension.
// Pure helpers, so the detection and parsing are unit tested.
import { common, createLowlight } from "lowlight";
import type { Element, Root, RootContent } from "hast";

export type DocKind = "markdown" | "html" | "code" | "image" | "table" | "text" | "binary";

const ext = (path: string) => (path.match(/\.([a-z0-9]{1,10})$/i)?.[1] || "").toLowerCase();
const base = (path: string) => path.slice(path.lastIndexOf("/") + 1).toLowerCase();

// Extension (or whole file name) to a highlight.js language in lowlight's
// common set.
const LANGUAGES: Record<string, string> = {
  json: "json",
  jsonl: "json",
  ndjson: "json",
  yaml: "yaml",
  yml: "yaml",
  toml: "ini",
  ini: "ini",
  cfg: "ini",
  conf: "ini",
  env: "bash",
  sh: "bash",
  bash: "bash",
  zsh: "bash",
  py: "python",
  rs: "rust",
  ts: "typescript",
  tsx: "typescript",
  mts: "typescript",
  js: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  go: "go",
  sql: "sql",
  css: "css",
  scss: "scss",
  less: "less",
  xml: "xml",
  svg: "xml",
  html: "xml",
  htm: "xml",
  java: "java",
  kt: "kotlin",
  swift: "swift",
  c: "c",
  h: "c",
  cc: "cpp",
  cpp: "cpp",
  hpp: "cpp",
  cs: "csharp",
  rb: "ruby",
  php: "php",
  lua: "lua",
  pl: "perl",
  r: "r",
  graphql: "graphql",
  gql: "graphql",
  diff: "diff",
  patch: "diff",
  makefile: "makefile",
  mk: "makefile",
};

const IMAGES: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  svg: "image/svg+xml",
};

const CODE_TYPES: Record<string, string> = {
  "application/json": "json",
  "application/x-ndjson": "json",
  "application/yaml": "yaml",
  "application/x-yaml": "yaml",
  "text/yaml": "yaml",
  "application/toml": "ini",
  "application/xml": "xml",
  "text/xml": "xml",
  "text/css": "css",
  "text/javascript": "javascript",
  "application/javascript": "javascript",
  "application/x-sh": "bash",
  "text/x-python": "python",
  "application/sql": "sql",
};

/** The highlight language for a path (or content type), or "" for none. */
export function codeLanguage(path: string, contentType = ""): string {
  const ct = mime(contentType);
  return CODE_TYPES[ct] || LANGUAGES[ext(path)] || LANGUAGES[base(path)] || "";
}

/** The image type a path or content type names, or "". */
export function imageType(path: string, contentType = ""): string {
  const ct = mime(contentType);
  return ct.startsWith("image/") ? ct : IMAGES[ext(path)] || "";
}

const mime = (ct: string) => ct.split(";")[0].trim().toLowerCase();

/** Which renderer a file gets. Binary content wins over its name. */
export function docKind(path: string, content: string, contentType = ""): DocKind {
  const ct = mime(contentType);
  const e = ext(path);
  if (ct.startsWith("image/") || (!ct && IMAGES[e]))
    return imageSrc(path, content, ct) ? "image" : isBinary(content) ? "binary" : "text";
  if (isBinary(content)) return "binary";
  if (ct === "text/markdown" || (!ct && ["md", "markdown"].includes(e))) return "markdown";
  if (ct === "text/html" || (!ct && ["html", "htm"].includes(e))) return "html";
  if (["text/csv", "text/tab-separated-values"].includes(ct) || (!ct && ["csv", "tsv"].includes(e)))
    return "table";
  if (codeLanguage(path, ct)) return "code";
  return "text";
}

/** NUL bytes, or many replacement characters from a lossy decode. */
export function isBinary(content: string): boolean {
  const head = content.slice(0, 8192);
  if (head.includes("\0")) return true;
  const bad = head.split("�").length - 1;
  return head.length > 0 && bad / head.length > 0.05;
}

/** A data URL for an image doc: a stored data URL, base64 text, or SVG text.
 * SVG is only ever shown through <img>, where its scripts never run. */
export function imageSrc(path: string, content: string, contentType = ""): string | null {
  const type = imageType(path, contentType);
  if (!type) return null;
  const text = content.trim();
  if (/^data:image\/[a-z0-9.+-]+(;[a-z0-9=.+-]+)*,/i.test(text)) return text;
  if (type === "image/svg+xml" && /<svg[\s>]/i.test(text))
    return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(text)}`;
  const b64 = text.replace(/\s+/g, "");
  if (b64.length >= 8 && b64.length % 4 === 0 && /^[A-Za-z0-9+/]+={0,2}$/.test(b64))
    return `data:${type};base64,${b64}`;
  return null;
}

/** JSON pretty-printed with two spaces; anything unparseable is unchanged. */
export function prettyJson(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

/** CSV or TSV rows. Quoted fields may hold the separator, quotes ("") and
 * line breaks; a trailing newline adds no empty row. */
export function parseDelimited(text: string, sep: "," | "\t"): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let quoted = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quoted) {
      if (c === '"' && text[i + 1] === '"') {
        field += '"';
        i++;
      } else if (c === '"') quoted = false;
      else field += c;
    } else if (c === '"' && field === "") quoted = true;
    else if (c === sep) {
      row.push(field);
      field = "";
    } else if (c === "\n" || c === "\r") {
      if (c === "\r" && text[i + 1] === "\n") i++;
      row.push(field);
      rows.push(row);
      row = [];
      field = "";
    } else field += c;
  }
  if (field !== "" || row.length) {
    row.push(field);
    rows.push(row);
  }
  return rows;
}

export type Token = { text: string; className?: string };

const lowlight = createLowlight(common);
// Highlighting stops here; larger files are numbered plain lines.
const HIGHLIGHT_CHARS = 256 * 1024;

/** The file's lines as highlighted tokens (plain when the language is
 * unknown or the file is large). A trailing newline adds no empty line. */
export function highlightLines(text: string, language: string): Token[][] {
  const body = text.endsWith("\n") ? text.slice(0, -1) : text;
  let tokens: Token[] = [{ text: body }];
  if (language && lowlight.registered(language) && body.length <= HIGHLIGHT_CHARS) {
    tokens = [];
    flatten(lowlight.highlight(language, body), [], tokens);
  }
  const lines: Token[][] = [[]];
  for (const t of tokens) {
    t.text.split("\n").forEach((part, i) => {
      if (i) lines.push([]);
      if (part) lines[lines.length - 1].push({ text: part, className: t.className });
    });
  }
  return lines;
}

function flatten(node: Root | RootContent, classes: string[], out: Token[]) {
  if (node.type === "text") {
    out.push({ text: node.value, className: classes.join(" ") || undefined });
  } else if (node.type === "root" || node.type === "element") {
    const own = node.type === "element" ? classNames(node) : [];
    for (const c of node.children) flatten(c, [...classes, ...own], out);
  }
}

function classNames(el: Element): string[] {
  const c = el.properties?.className;
  return Array.isArray(c) ? c.map(String) : typeof c === "string" ? [c] : [];
}
