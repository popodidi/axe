// A table of contents (components/ui/toc.tsx, on the doc page and rendered
// run prompts): heading ids for rendered markdown, the entries to list and
// the section the reader is in.

// A heading's anchor like GitHub's: lower case, punctuation dropped, each
// space a "-".
export function slugify(text: string) {
  return (
    text
      .toLowerCase()
      .trim()
      .replace(/[^\p{L}\p{N}\s_-]/gu, "")
      .replace(/\s/g, "-") || "section"
  );
}

// Minimal hast shapes (the rehype tree) this plugin reads.
type HastNode = {
  type: string;
  tagName?: string;
  value?: string;
  properties?: Record<string, unknown>;
  children?: HastNode[];
};

function textOf(n: HastNode): string {
  if (n.type === "text") return n.value || "";
  return (n.children || []).map(textOf).join("");
}

export const TOC_LEVELS = ["h1", "h2", "h3"];

// A rehype plugin that gives every h1-h3 a unique id from its text, with
// `prefix` before it (a folder doc's files share one page). It runs after
// rehype-sanitize, so the ids are not renamed.
export function rehypeHeadingIds({ prefix = "" }: { prefix?: string } = {}) {
  return (tree: HastNode) => {
    const seen = new Map<string, number>();
    const walk = (n: HastNode) => {
      if (n.type === "element" && n.tagName && TOC_LEVELS.includes(n.tagName)) {
        const base = prefix + slugify(textOf(n));
        const count = seen.get(base) || 0;
        seen.set(base, count + 1);
        n.properties = { ...n.properties, id: count ? `${base}-${count}` : base };
      }
      n.children?.forEach(walk);
    };
    walk(tree);
  };
}

// The current section: the last heading whose top has passed `line` (px from
// the viewport top), the first before any has, the last at the page's end.
export function activeHeading(tops: number[], line: number, atEnd: boolean) {
  if (!tops.length) return -1;
  if (atEnd) return tops.length - 1;
  let i = 0;
  tops.forEach((t, j) => {
    if (t <= line) i = j;
  });
  return i;
}

export type TocHeading = { id: string; level: number; text: string };
export type TocEntry = TocHeading & { depth: number; number: string };

// Fewer entries than this and a doc is short: no contents.
export const TOC_MIN = 4;

// A heading that carries its own number ("1. Objective", "4.2 Plan").
const NUMBERED = /^\s*\d+(\.\d+)*[.):]?\s/;

// The listed entries: a lone leading top-level heading is the title and is
// left out; nothing for a short doc. Depth is relative to the top listed
// level and never skips one. Entries are numbered 1, 1.1, ... unless the
// headings bring their own numbers.
export function tocEntries(headings: TocHeading[], min = TOC_MIN): TocEntry[] {
  let hs = headings;
  if (hs.length > 1) {
    const top = Math.min(...hs.map((h) => h.level));
    if (hs[0].level === top && hs.filter((h) => h.level === top).length === 1) hs = hs.slice(1);
  }
  if (hs.length < min) return [];
  const base = Math.min(...hs.map((h) => h.level));
  const own = hs.some((h) => NUMBERED.test(h.text));
  const counters: number[] = [];
  return hs.map((h) => {
    const depth = Math.min(h.level - base, counters.length);
    counters.length = depth + 1;
    counters[depth] = (counters[depth] || 0) + 1;
    return { ...h, depth, number: own ? "" : counters.join(".") };
  });
}

// The h1-h3 of a parsed HTML document (a stored HTML doc), outside its own
// navigation, each given a unique id if it has none and marked with
// `data-axe-toc` for the frame's scroll spy. Returns no headings when the
// doc brings its own contents (a nav with several in-page links).
export function markHtmlHeadings(d: Document): TocHeading[] {
  const own = Array.from(d.querySelectorAll("nav")).some(
    (n) => n.querySelectorAll('a[href^="#"]').length >= 3,
  );
  if (own) return [];
  const taken = new Set(Array.from(d.querySelectorAll("[id]")).map((e) => e.id));
  const heads = Array.from(d.body?.querySelectorAll<HTMLElement>(TOC_LEVELS.join(",")) || []).filter(
    (e) => !e.closest("nav"),
  );
  return heads.map((e) => {
    const text = (e.textContent || "").replace(/\s+/g, " ").trim();
    if (!e.id) {
      const base = slugify(text);
      let id = base;
      for (let n = 1; taken.has(id); n++) id = `${base}-${n}`;
      taken.add(id);
      e.id = id;
    }
    e.setAttribute("data-axe-toc", "");
    return { id: e.id, level: Number(e.tagName.slice(1)), text };
  });
}
