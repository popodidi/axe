import { test, expect } from "@playwright/test";
import { choose, manyTasks, mockDesk, tasks } from "./fixtures";

test("keyboard sorting survives filters, automatic refresh and reload", async ({ page }) => {
  const list = tasks.map(t => ({ ...t }));
  await mockDesk(page, list);
  await page.goto("/?server=http://axe.test&state=all");
  const ids = () => page.locator("tbody tr").evaluateAll(rows => rows.map(row => Number(row.id.replace("task-", ""))));
  await expect.poll(ids).toEqual([8, 7, 6, 5, 4, 3, 2, 1]);
  await expect(page.locator('th[aria-sort="none"]')).toHaveCount(5);
  await page.getByRole("button", { name: "Sort by ID", exact: true }).focus();
  await page.keyboard.press("Enter");
  await expect.poll(ids).toEqual([1, 2, 3, 4, 5, 6, 7, 8]);
  const header = page.getByRole("columnheader", { name: "Sort by ID" });
  await expect(header).toHaveAttribute("aria-sort", "ascending");
  await page.keyboard.press("Space");
  await expect.poll(ids).toEqual([8, 7, 6, 5, 4, 3, 2, 1]);
  await choose(page.getByLabel("project", { exact: true }), "atlas");
  await expect.poll(ids).toEqual([8, 4, 3, 1]);
  list.push({ ...tasks[0], id: 99, title: "Refreshed task" });
  await expect.poll(ids, { timeout: 20000 }).toEqual([99, 8, 4, 3, 1]);
  await expect(header).toHaveAttribute("aria-sort", "descending");
  await page.getByLabel("Search tasks").fill("Refreshed");
  await expect.poll(ids).toEqual([99]);
  await page.reload();
  await expect.poll(ids).toEqual([99]);
  await expect(page.getByLabel("Sort", { exact: true })).toHaveAttribute("data-value", "id:desc");
});

test("sorting precedes pagination and retains legacy controls", async ({ page }) => {
  await mockDesk(page, manyTasks);
  await page.goto("/?server=http://axe.test&state=all&page=2");
  await page.getByRole("button", { name: "Sort by ID", exact: true }).click();
  await expect(page.locator("tbody tr").first()).toHaveAttribute("id", "task-1");
  await expect(page.getByText("38 results · Page 1 / 2")).toBeVisible();
  await page.getByRole("button", { name: "Next", exact: true }).click();
  await expect(page.locator("tbody tr").first()).toHaveAttribute("id", "task-117");
  await choose(page.getByLabel("Sort", { exact: true }), "oldest");
  await expect(page.getByRole("columnheader", { name: "Sort by Created" })).toHaveAttribute("aria-sort", "ascending");
});
