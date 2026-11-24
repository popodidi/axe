// A doc's table of contents (the doc page, app/doc): heading ids for the
// rendered markdown and the section the reader is in.

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
