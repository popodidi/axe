import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
const read = (path: string) =>
  readFileSync(fileURLToPath(new URL(path, import.meta.url)), "utf8");
describe("design tokens", () => {
  it("app brand tokens are identical to docs/quiet-design-tokens.css", () => {
    // Fix drift with: cp docs/quiet-design-tokens.css infra/dashboard/app/styles/brand-tokens.css
    expect(read("../app/styles/brand-tokens.css")).toBe(
      read("../../../docs/quiet-design-tokens.css"),
    );
  });
});
