import { test, expect } from "@playwright/test";
import { mockDesk } from "./fixtures";

test("artifacts deduplicate displayed values while events retain every record", async ({ page }) => {
  await mockDesk(page);
  const values = ["https://example.com/pr/1", "local evidence", "https://example.com/pr/1", "https://example.com/pr/1/", "local evidence"];
  const events = values.map((body, i) => ({
    id: i + 1, at: "2026-10-30T00:00:00Z", actor: "agent",
    kind: "artifact", body, from_value: null, to_value: null,
  }));
  await page.route("http://axe.test/api/tasks/3/events", route => route.fulfill({
    json: events, headers: { "Access-Control-Allow-Origin": "*" },
  }));
  await page.goto("/?server=http://axe.test&task=3");
  const detail = page.getByLabel("Task detail", { exact: true });
  // Artifacts have their own tab.
  await detail.getByRole("tab", { name: "artifacts", exact: true }).click();
  await expect(page).toHaveURL(/tab=artifacts/);
  const artifacts = detail.getByRole("table", { name: "Artifact references" });
  const items = artifacts.locator("tbody tr");
  await expect(items).toHaveCount(3);
  for (const [i, v] of [values[3], ...values.slice(0, 2).reverse()].entries())
    await expect(items.nth(i)).toContainText(v);
  await expect(artifacts.getByRole("link")).toHaveCount(2);
  await expect(artifacts.getByRole("link").first()).toHaveAttribute("href", values[3]);
  await expect(artifacts.getByRole("link").first()).toHaveAttribute("rel", "noopener noreferrer");
  await detail.getByRole("tab", { name: "events", exact: true }).click();
  await expect(detail.locator("article")).toHaveCount(events.length);
  for (const [i, event] of [...events].reverse().entries())
    await expect(detail.locator("article").nth(i)).toContainText(event.body);
});
