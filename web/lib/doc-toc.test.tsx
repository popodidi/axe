import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import Markdown from "../app/markdown";
import { activeHeading, slugify } from "./doc-toc";

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
});
