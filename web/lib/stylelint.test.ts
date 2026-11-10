import { fileURLToPath } from "node:url";
import stylelint from "stylelint";
import { describe, expect, it } from "vitest";
import config from "../stylelint.config.mjs";
const feature = fileURLToPath(
  new URL("../components/dashboard/probe.module.css", import.meta.url),
);
const breakpoints = fileURLToPath(
  new URL("../app/styles/breakpoints.module.css", import.meta.url),
);
async function errors(code: string, codeFilename = feature) {
  const { results } = await stylelint.lint({ code, config, codeFilename });
  return results[0].warnings.map((w) => w.rule);
}
describe("design-system style lint", () => {
  it.each([
    "padding: 13px",
    "margin: -13px",
    "padding: 13PX",
    "margin: 0 -2Px",
    "width: calc(var(--space-1) + 2px)",
    "padding: 8px",
    "border-width: 0.5px",
    "padding: 1.3e1px",
    "width: 1e2px",
    "height: 1E3PX",
    "margin: +13px",
    "padding: 13\\70 x",
    "padding: 13p\\78",
    "--gap: 13px",
    "grid-template-columns: repeat(2, minmax(0, 13px))",
  ])("rejects off-token px: %s", async (decl) => {
    expect(await errors(`.a { ${decl}; }`)).toContain(
      "harvis/no-off-token-px",
    );
  });
  it.each([
    "padding: 0",
    "margin: 0px",
    "border: 1px solid var(--surface-3)",
    "margin-top: -1px",
    "width: calc(var(--text-body) - 1px)",
    "padding: var(--space-1) var(--space-2)",
    "border-width: 1e0px",
    "background: url(icon-13px.svg)",
    'content: "13px"',
  ])("allows tokens and hairlines: %s", async (decl) => {
    expect(await errors(`.a { ${decl}; }`)).toEqual([]);
  });
  it.each(["color: #fff", "color: white", "background: rgba(0, 0, 0, 0.1)"])(
    "rejects color literals: %s",
    async (decl) => {
      expect(await errors(`.a { ${decl}; }`)).not.toEqual([]);
    },
  );
  it.each([
    "@media (max-width: 13px) { .a { color: inherit; } }",
    "@media (min-width: 1.3e1px) and (max-width: 760PX) { .a { color: inherit; } }",
    "@container (max-width: 640px) { .a { color: inherit; } }",
    "@container list (inline-size < -2px) { .a { color: inherit; } }",
    "@value bp-probe: (max-width: 13px);",
  ])("rejects px in query conditions outside the breakpoints file: %s", async (code) => {
    expect(await errors(code)).toContain("harvis/no-off-token-px");
  });
  it("allows breakpoint tokens in feature queries", async () => {
    expect(
      await errors(
        '@value bp-phone from "../../app/styles/breakpoints.module.css";\n@media bp-phone { .a { padding: var(--space-1); } }',
      ),
    ).toEqual([]);
  });
  it("allows px only in @value breakpoints inside the breakpoints file", async () => {
    expect(
      await errors("@value bp-probe: (max-width: 760px);", breakpoints),
    ).toEqual([]);
    expect(
      await errors("@media (max-width: 13px) { .a { color: inherit; } }", breakpoints),
    ).toContain("harvis/no-off-token-px");
    expect(await errors(".a { padding: 13px; }", breakpoints)).toContain(
      "harvis/no-off-token-px",
    );
  });
  it("lints the real breakpoints file cleanly", async () => {
    const { results } = await stylelint.lint({ files: breakpoints, config });
    expect(results[0].warnings).toEqual([]);
  });
});
