import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import Markdown from "../app/markdown";
import { activeHeading, slugify, tocEntries, type TocHeading } from "./doc-toc";

describe("doc contents", () => {
  it("slugs headings like GitHub", () => {
    expect(slugify("Plan & Verify (v2)")).toBe("plan--verify-v2");
    expect(slugify("  Über café  ")).toBe("über-café");
    expect(slugify("!!!")).toBe("section");
  });

  it("gives h1-h3 unique ids only when asked", () => {
    const body = "# Title\n\n## Plan\n\n## Plan\n\n### Step `one`\n\n#### Deep\n\n[jump](#plan)";
    const html = renderToStaticMarkup(<Markdown body={body} headingIds="" />);
    expect(html).toContain('<h1 id="title">');
    expect(html).toContain('<h2 id="plan">');
    expect(html).toContain('<h2 id="plan-1">');
    expect(html).toContain('<h3 id="step-one">');
    expect(html).toContain("<h4>Deep</h4>");
    // In-page anchors stay in the page.
    expect(html).toContain('<a href="#plan">jump</a>');
    expect(renderToStaticMarkup(<Markdown body={body} headingIds="f1-" />)).toContain(
      '<h2 id="f1-plan-1">',
    );
    expect(renderToStaticMarkup(<Markdown body={body} />)).toContain("<h2>Plan</h2>");
  });

  it("follows the section the reader has scrolled to", () => {
    expect(activeHeading([], 100, false)).toBe(-1);
    expect(activeHeading([300, 900], 100, false)).toBe(0);
    expect(activeHeading([-500, 50, 400], 100, false)).toBe(1);
    expect(activeHeading([-500, 50, 400], 100, true)).toBe(2);
  });

  const h = (level: number, text: string): TocHeading => ({ id: slugify(text), level, text });

  it("lists a long doc's sections, numbered, without its title", () => {
    const entries = tocEntries([
      h(1, "Title"),
      h(2, "Plan"),
      h(3, "Step"),
      h(3, "Check"),
      h(2, "Verify"),
      h(2, "Deliver"),
    ]);
    expect(entries.map((e) => [e.text, e.depth, e.number])).toEqual([
      ["Plan", 0, "1"],
      ["Step", 1, "1.1"],
      ["Check", 1, "1.2"],
      ["Verify", 0, "2"],
      ["Deliver", 0, "3"],
    ]);
  });

  it("hides the contents of a short doc", () => {
    expect(tocEntries([h(1, "Title"), h(2, "A"), h(2, "B"), h(2, "C")])).toEqual([]);
    expect(tocEntries([h(2, "A"), h(2, "B"), h(2, "C"), h(2, "D")])).toHaveLength(4);
    expect(tocEntries([])).toEqual([]);
  });

  it("keeps headings' own numbers and never skips a level", () => {
    const own = tocEntries([h(2, "1. Objective"), h(2, "2. Scope"), h(3, "2.1 In"), h(2, "Appendix")]);
    expect(own.map((e) => e.number)).toEqual(["", "", "", ""]);
    // Two top headings: no title to drop. An h3 straight after an h1 is one level in.
    const deep = tocEntries([h(1, "A"), h(3, "A deep"), h(1, "B"), h(2, "B one")]);
    expect(deep.map((e) => [e.depth, e.number])).toEqual([
      [0, "1"],
      [1, "1.1"],
      [0, "2"],
      [1, "2.1"],
    ]);
  });
});
