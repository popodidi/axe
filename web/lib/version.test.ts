import { expect, test } from "vitest";
import { buildLine, COMMIT_BASE } from "./version";

const sha = "dfe3a939ab12cd34ef56ab78cd90ef12ab34cd56";

test("splits the version around its commit and links the whole commit", () => {
  expect(buildLine({ version: "0.5.3 (dfe3a939ab12)", commit: sha })).toEqual({
    release: "0.5.3",
    short: "dfe3a939ab12",
    href: COMMIT_BASE + sha,
  });
  expect(COMMIT_BASE).toBe("https://github.com/popodidi/axe/commit/");
});

test("a build without a commit shows the version alone", () => {
  expect(buildLine({ version: "0.5.3", commit: null })).toEqual({
    release: "0.5.3",
    short: null,
    href: null,
  });
});

test("never links a commit that does not match the version", () => {
  expect(
    buildLine({ version: "0.5.3 (dfe3a939ab12)", commit: "0".repeat(40) })?.href,
  ).toBeNull();
  expect(
    buildLine({ version: "0.5.3 (dfe3a939ab12)", commit: "javascript:x" })?.href,
  ).toBeNull();
});

test("anything that is not a version yields no line", () => {
  for (const v of [null, undefined, "0.5.3", {}, { version: "" }, { version: 3 }])
    expect(buildLine(v)).toBeNull();
});
