import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import Markdown from "./markdown";
describe("task context Markdown", () => {
  it("renders GFM, highlights code, and keeps external links safe", () => {
    const html = renderToStaticMarkup(
      <Markdown
        body={
          '## Plan\n\n| A | B |\n| - | - |\n| x | y |\n\n- [x] Complete\n\n```js\nconst x = "value";\n```\n\nhttps://example.com\n\n<script>alert(1)</script>\n\n[bad](javascript:alert(1))'
        }
      />,
    );
    expect(html).toContain("<h2>Plan</h2>");
    expect(html).toMatch(/<table class="[^"]+">/);
    expect(html).toMatch(/<pre class="[^"]+"><code class="hljs/);
    expect(html).toContain('type="checkbox"');
    expect(html).toContain("hljs-keyword");
    expect(html).toContain('target="_blank"');
    expect(html).toContain('rel="noopener noreferrer"');
    expect(html).not.toContain("<script>");
    expect(html).not.toContain('href="javascript:');
  });
});
