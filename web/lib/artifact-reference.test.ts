import { expect, test } from "vitest";
import { artifactReference } from "./artifact-reference";
test("URL labels never invent titles or trust lookalike hosts", () => {
  expect(artifactReference("https://github.com/org/repo/pull/779").name).toBe(
    "PR #779 · org/repo",
  );
  expect(
    artifactReference("https://github.com.evil.test/org/repo/pull/779").type,
  ).toBe("Link");
  expect(artifactReference("https://example.com/a%20report.png").name).toBe(
    "a report.png",
  );
  expect(artifactReference("https://example.com/%FF.png").name).toBe("%FF.png");
  expect(
    artifactReference("https://team.larksuite.com/record/rec123").name,
  ).toBe("Lark record rec123");
  expect(
    artifactReference("https://team.larksuite.com/base/synthetic?record=rec123")
      .name,
  ).toBe("Lark record rec123");
  for (const value of [
    "javascript:alert(1)",
    "/local/report.png",
    "",
    "not a URL",
  ]) {
    expect(artifactReference(value).href).toBeUndefined();
    expect(artifactReference(value).type).toBe("Reference");
  }
});
