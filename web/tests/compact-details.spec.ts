import { test, expect } from "@playwright/test";
import { compactFixture, references } from "./compact-fixture";
import { choose, mockDesk, runs } from "./fixtures";
import { runCommands } from "../lib/model";
const base = "/?server=http://axe.test&task=3";
test("run disclosure preserves full results and exact commands with mouse and keyboard", async ({
  page,
}) => {
  await compactFixture(page);
  await page.addInitScript(() =>
    Object.defineProperty(navigator, "clipboard", {
      value: {
        writeText: async (text: string) => {
          (window as unknown as { copied: string }).copied = text;
        },
      },
    }),
  );
  await page.goto(base + "&tab=runs");
  const run = page.getByRole("article", { name: "Run #10", exact: true });
  const toggle = run.getByRole("button", { name: "Run #10 details" });
  await expect(toggle).toHaveAttribute("aria-expanded", "false");
  await expect(run.getByRole("button", { name: "Copy attach" })).toBeHidden();
  await toggle.focus();
  await expect(toggle).toHaveCSS("outline-style", "solid");
  await page.keyboard.press("Enter");
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await expect(run.locator("p")).toHaveText(
    "Finished process, task still awaiting review. " +
      "Full recorded result. ".repeat(30),
  );
  await expect(run.getByText("exit 0", { exact: true })).toBeVisible();
  for (const [label, value] of Object.entries(
    runCommands({ ...runs[1], task_id: 3 }),
  )) {
    if (!value) continue;
    await run
      .getByRole("button", { name: `Copy ${label}`, exact: true })
      .click();
    expect(
      await page.evaluate(
        () => (window as unknown as { copied: string }).copied,
      ),
    ).toBe(value);
  }
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-expanded", "false");
  await toggle.focus();
  await page.keyboard.press("Space");
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await expect(
    page.getByRole("heading", {
      name: "Make dashboard Overview a desk page",
      exact: true,
    }),
  ).toHaveCount(1);
});
test("artifact search, types, date order, lossless details and copy", async ({
  page,
}) => {
  await compactFixture(page);
  await page.addInitScript(() =>
    Object.defineProperty(navigator, "clipboard", {
      value: {
        writeText: async (text: string) => {
          (window as unknown as { copied: string }).copied = text;
        },
      },
    }),
  );
  await page.goto(base + "&tab=artifacts");
  const table = page.getByRole("table", { name: "Artifact references" });
  await expect(table.locator("tbody tr")).toHaveCount(references.length);
  await expect(table.locator("tbody tr").first()).toContainText(
    references.at(-2)!,
  );
  await expect(table.locator("tbody tr").last()).toContainText(
    "local evidence",
  );
  for (const [type, label, value] of [
    ["PR", "PR #779 · example/project", references[0]],
    ["CI", "CI run 123456 · example/project", references[1]],
    ["Lark", "Lark record rec123", references[2]],
    ["Link", references.at(-2)!, references.at(-2)!],
  ]) {
    await choose(page.getByLabel("Artifact type"), type);
    const link = table.getByRole("link", { name: label, exact: true });
    await expect(link).toHaveAttribute("href", value);
    await table
      .getByLabel(`Reference details: ${label}`, { exact: true })
      .focus();
    await page.keyboard.press("Enter");
    await expect(table.locator("[data-reference-details]")).toContainText(value);
    await expect(table.locator("[data-reference-details]")).toContainText(
      "synthetic-agent",
    );
    await table
      .getByRole("button", { name: `Copy reference: ${label}`, exact: true })
      .click();
    expect(
      await page.evaluate(
        () => (window as unknown as { copied: string }).copied,
      ),
    ).toBe(value);
  }
  await choose(page.getByLabel("Artifact type"), "File");
  await expect(table.locator("tbody tr")).toHaveCount(18);
  await page.getByLabel("Search artifacts").fill("layout-17");
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await expect(table.getByRole("link")).toHaveText("layout-17.png");
  await page.getByLabel("Search artifacts").fill("no such artifact");
  await expect(page.getByText("No matching artifacts.")).toBeVisible();
});
test("empty runs and artifacts stay honest", async ({ page }) => {
  await mockDesk(page);
  await page.route("http://axe.test/api/runs?*", (r) =>
    r.fulfill({ json: [] }),
  );
  await page.goto(base + "&tab=runs");
  await expect(page.getByText("No runs", { exact: true })).toBeVisible();
  await page.getByRole("tab", { name: "artifacts", exact: true }).click();
  await expect(
    page.getByText("No artifacts recorded for this task."),
  ).toBeVisible();
});
