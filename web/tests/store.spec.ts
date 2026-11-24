import { test, expect, type Page, type Route } from "@playwright/test";
import { mockDesk } from "./fixtures";

// Synthetic store fixtures: signed links, a read session, blob refs and docs.
const sha = (c: string) => c.repeat(64);
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
  "base64",
);
const future = Math.floor(Date.UTC(2099, 0, 1) / 1000);
const past = Math.floor(Date.UTC(2026, 0, 1) / 1000);
const link = (s: string, name: string, exp: number, sig = "f00d") =>
  `http://axe.test/b/${s}/${name}?exp=${exp}&sig=${sig}`;
const legacy = "http://files.lan/run/dispatch%3Aab/manifest.json";
const cors = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Headers": "authorization, content-type",
  "Access-Control-Allow-Methods": "GET, POST",
};
const blobs: Record<string, { type: string; body: string | Buffer }> = {
  [sha("1")]: { type: "image/png", body: PNG },
  [sha("2")]: {
    type: "text/html; charset=utf-8",
    body: "<h1>Report</h1><script>parent.document.title = 'escaped'</script>",
  },
  [sha("3")]: { type: "text/plain; charset=utf-8", body: "hello from the store" },
  [sha("4")]: { type: "application/json", body: '{"files": 2}' },
  [sha("5")]: { type: "text/tab-separated-values", body: "url\tsha256\n" },
};

// A long doc with frontmatter, a wide code block and a closing line.
const longDocPath = "research/long-report.md";
const longDoc = [
  "---",
  'title: "Long report"',
  "status: draft",
  "---",
  "# Long report",
  ...Array.from({ length: 60 }, (_, i) => `Paragraph ${i + 1} of the long report.`),
  "```",
  `wide ${"x".repeat(400)}`,
  ...Array.from({ length: 40 }, (_, i) => `line ${i + 1}`),
  "```",
  "The end of the long report.",
].join("\n\n");

type Store = {
  read: boolean;
  tokens: string[];
  signed: string[];
};

async function mockStore(page: Page, read: boolean) {
  const store: Store = { read, tokens: [], signed: [] };
  await page.route("http://axe.test/**", async (route: Route) => {
    const req = route.request();
    const url = new URL(req.url());
    if (req.method() === "OPTIONS")
      return route.fulfill({ status: 204, headers: cors });
    const json = (body: unknown, status = 200) =>
      route.fulfill({ status, json: body, headers: cors });
    const p = url.pathname;
    if (p === "/api/session") {
      if (req.method() === "POST") {
        const auth = req.headers()["authorization"] || "";
        store.tokens.push(auth);
        if (auth !== "Bearer secret-token")
          return json({ error: "unauthorized" }, 401);
        store.read = true;
      }
      return json({ read: store.read, expires_at: store.read ? "2099-01-01T00:00:00Z" : null });
    }
    if (p.startsWith("/api/blobs") || p.startsWith("/api/docs") || p.startsWith("/api/store")) {
      if (!store.read) return json({ error: "unauthorized" }, 401);
      const signedLink = p.match(/^\/api\/blobs\/([0-9a-f]{64})\/link$/);
      if (signedLink && req.method() === "POST") {
        const name = (req.postDataJSON() as { name?: string }).name || "";
        store.signed.push(signedLink[1]);
        return json({
          path: `/b/${signedLink[1]}${name ? "/" + encodeURIComponent(name) : ""}?exp=${future}&sig=fresh`,
        });
      }
      if (p === "/api/blobs/resolve")
        return url.searchParams.get("url") === legacy
          ? json({ sha256: sha("4"), size: 12, content_type: "application/json", name: "manifest.json" })
          : json({ error: "no stored blob for this url" }, 404);
      if (p === "/api/blobs")
        return json([
          { sha256: sha("1"), size: PNG.length, content_type: "image/png", created_at: "2026-11-19T01:00:00Z", names: ["shot.png"], tasks: [3] },
          { sha256: sha("5"), size: 329023, content_type: "text/tab-separated-values", created_at: "2026-11-19T01:10:18Z", names: ["dufs-artifact-map.tsv"], tasks: [1241] },
          { sha256: sha("3"), size: 20, content_type: "text/plain; charset=utf-8", created_at: "2026-11-18T00:00:00Z", names: [], tasks: [] },
        ]);
      if (p === "/api/docs")
        return json([
          { path: "harvis/docs/reference/brand-guide.md", size: 120, version: "c".repeat(40), updated_at: "2026-11-10T00:00:00+00:00" },
          { path: "skills/axe/SKILL.md", size: 900, version: "d".repeat(40), updated_at: "2026-11-11T00:00:00+00:00" },
        ]);
      if (p === "/api/store/stats")
        // Sums over the whole store, more than the three listed rows.
        return json({
          total: { count: 1862, bytes: 360 * 1024 * 1024 },
          recent: { count: 41, bytes: 12 * 1024 * 1024 },
          recent_days: 7,
          recent_since: "2026-11-16T00:00:00Z",
          types: [
            { content_type: "image/png", count: 700, bytes: 200 * 1024 * 1024 },
            { content_type: "video/webm", count: 60, bytes: 90 * 1024 * 1024 },
          ],
          other_types: { count: 1102, bytes: 70 * 1024 * 1024 },
          unreferenced: { count: 15, bytes: 2048 },
          docs: { count: 2, bytes: 1020 },
        });
      if (p === "/api/docs/attachments")
        return json([{ scope: "label:harvis", path: "harvis/docs/reference/brand-guide.md", exists: true }]);
      if (p === "/api/docs/log")
        return json([
          { version: "c".repeat(40), at: "2026-11-10T00:00:00+00:00", author: "axe", message: "edit" },
          { version: "e".repeat(40), at: "2026-11-01T00:00:00+00:00", author: "axe", message: "create" },
        ]);
      if (p === "/api/docs/file")
        return json({
          path: url.searchParams.get("path"),
          version: "c".repeat(40),
          kind: "doc",
          content: url.searchParams.get("path") === longDocPath ? longDoc : "# Quiet\n\nGroup with **space** first.",
        });
    }
    const signedPath = p.match(/^\/b\/([0-9a-f]{64})(\/[^/]+)?$/);
    if (signedPath) {
      const exp = Number(url.searchParams.get("exp"));
      if (exp * 1000 <= Date.now())
        return route.fulfill({ status: 403, json: { error: "link expired" }, headers: cors });
      const b = blobs[signedPath[1]];
      return route.fulfill({
        status: 200,
        body: b.body,
        headers: { ...cors, "Content-Type": b.type, "Content-Security-Policy": "sandbox" },
      });
    }
    return route.fallback();
  });
  return store;
}

async function artifactsTab(page: Page, values: string[]) {
  await page.route("http://axe.test/api/tasks/3/events", (route) =>
    route.fulfill({
      json: values.map((body, i) => ({
        id: i + 1, at: `2026-11-1${i}T00:00:00Z`, actor: "agent",
        kind: "artifact", body, from_value: null, to_value: null,
      })),
      headers: cors,
    }),
  );
  await page.goto("/?server=http://axe.test&task=3&tab=artifacts");
  return page.getByLabel("Task detail", { exact: true });
}

test("store artifacts preview inline; HTML stays sandboxed", async ({ page }) => {
  await mockDesk(page);
  await mockStore(page, false);
  const detail = await artifactsTab(page, [
    link(sha("1"), "shot.png", future),
    link(sha("2"), "report.html", future),
  ]);
  const table = detail.getByRole("table", { name: "Artifact references" });
  await expect(table.getByRole("cell", { name: /^Store$/ })).toHaveCount(2);
  // A valid link opens without unlocking the store.
  await detail.getByRole("button", { name: "Preview: shot.png" }).click();
  const img = detail.getByRole("img", { name: "Preview of shot.png" });
  await expect(img).toBeVisible();
  await expect(img).toHaveAttribute("src", link(sha("1"), "shot.png", future));
  expect(await img.evaluate((e: HTMLImageElement) => e.naturalWidth)).toBe(1);
  await detail.getByRole("button", { name: "Preview: report.html" }).click();
  const frame = detail.locator("iframe[title='Preview of report.html']");
  await expect(frame).toHaveAttribute("sandbox", "");
  await expect(
    page.frameLocator("iframe[title='Preview of report.html']").getByRole("heading", { name: "Report" }),
  ).toBeVisible();
  expect(await page.title()).not.toBe("escaped");
  await detail.getByRole("button", { name: "Preview: shot.png" }).click();
  await expect(img).toHaveCount(0);
});

test("an expired link re-signs through the server after unlocking", async ({ page }) => {
  await mockDesk(page);
  const store = await mockStore(page, false);
  const detail = await artifactsTab(page, [
    link(sha("3"), "notes.txt", past),
    legacy,
  ]);
  await detail.getByRole("button", { name: "Preview: notes.txt" }).click();
  const preview = detail.locator("[data-artifact-preview]");
  await expect(preview).toContainText("This link expired");
  const form = preview.getByRole("form", { name: "Unlock the store" });
  await form.getByLabel("axe token").fill("wrong");
  await form.getByRole("button", { name: "Unlock" }).click();
  await expect(form).toContainText("The server rejected this token.");
  await form.getByLabel("axe token").fill("secret-token");
  await form.getByRole("button", { name: "Unlock" }).click();
  await expect(preview).toContainText("hello from the store");
  expect(store.tokens).toEqual(["Bearer wrong", "Bearer secret-token"]);
  expect(store.signed).toEqual([sha("3")]);
  await expect(preview.getByRole("link", { name: "Download" })).toHaveAttribute(
    "href",
    `http://axe.test/b/${sha("3")}/notes.txt?exp=${future}&sig=fresh`,
  );
  // A recorded file URL from before the store resolves to its blob.
  await detail.getByRole("button", { name: "Preview: manifest.json" }).click();
  await expect(detail.locator("[data-artifact-preview]").filter({ hasText: "manifest.json" })).toContainText('{"files": 2}');
  expect(store.signed).toEqual([sha("3"), sha("4")]);
});

test("the store page lists, searches and opens blobs and docs", async ({ page }) => {
  await mockDesk(page);
  await mockStore(page, false);
  await page.goto("/?server=http://axe.test&view=store");
  const unlock = page.getByRole("form", { name: "Unlock the store" });
  await unlock.getByLabel("axe token").fill("secret-token");
  await unlock.getByRole("button", { name: "Unlock" }).click();
  const blobsTable = page.getByRole("table", { name: "Blobs" });
  await expect(blobsTable.locator("tbody tr")).toHaveCount(3);
  // The stats row shows the server's sums, not the listed rows.
  const stats = page.getByRole("region", { name: "Store stats" });
  await expect(stats).toContainText("360 MiB");
  await expect(stats).toContainText("1,862 blobs");
  await expect(stats).toContainText("Added in 7 days");
  await expect(stats).toContainText("41 blobs");
  await expect(stats).toContainText("image/png 200 MiB");
  await expect(stats).toContainText("other 70 MiB");
  await expect(stats).toContainText("2.0 KiB no task event links");
  await expect(stats.getByText("Docs", { exact: true })).toBeVisible();
  // Newest first.
  await expect(blobsTable.locator("tbody tr").first()).toContainText("dufs-artifact-map.tsv");
  await page.getByLabel("Search blobs").fill("#1241");
  await expect(page).toHaveURL(/q=%231241/);
  await expect(blobsTable.locator("tbody tr")).toHaveCount(1);
  await expect(blobsTable).toContainText("321 KiB");
  await page.getByLabel("Search blobs").fill("shot");
  await blobsTable.getByRole("button", { name: "shot.png" }).click();
  await expect(page).toHaveURL(new RegExp(`blob=${sha("1")}`));
  // The item opens in the side pane with its metadata and a full link; the
  // list stays. Signing in is the portal's, so the store has no Lock.
  const pane = page.getByRole("region", { name: "Store item" });
  await expect(pane.getByRole("img", { name: "Preview of shot.png" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Lock" })).toHaveCount(0);
  const blobFacts = pane.getByRole("definition");
  await expect(pane.locator("dl[aria-label='Blob details']")).toContainText(sha("1"));
  await expect(pane.locator("dl[aria-label='Blob details']")).toContainText("image/png");
  await expect(pane.locator("dl[aria-label='Blob details']")).toContainText("2099");
  await expect(blobFacts.getByRole("button", { name: "#3" })).toBeVisible();
  const full = pane.getByRole("link", { name: "Open full" });
  await expect(full).toHaveAttribute("href", `http://axe.test/b/${sha("1")}/shot.png?exp=${future}&sig=fresh`);
  await expect(full).toHaveAttribute("target", "_blank");
  await expect(blobsTable).toBeVisible();
  await expect(blobsTable.locator("tbody tr[aria-selected=true]")).toContainText("shot.png");
  // Escape closes it and focus returns to the row.
  await page.keyboard.press("Escape");
  await expect(page.locator("[data-store-viewer]")).toHaveCount(0);
  await expect(page).not.toHaveURL(/blob=/);
  await expect(blobsTable.getByRole("button", { name: "shot.png" })).toBeFocused();
  await blobsTable.getByRole("button", { name: "shot.png" }).click();
  await pane.getByRole("button", { name: "Close" }).click();
  await expect(page.locator("[data-store-viewer]")).toHaveCount(0);

  await page.getByRole("tab", { name: "docs" }).click();
  const tree = page.getByRole("list", { name: "Docs" });
  await page.getByLabel("Search docs").fill("label:harvis");
  // A search opens every folder holding a match.
  await expect(tree.getByRole("button", { name: "reference/" })).toHaveAttribute("aria-expanded", "true");
  await expect(tree.getByRole("button", { name: "SKILL.md" })).toHaveCount(0);
  await tree.getByRole("button", { name: "brand-guide.md" }).click();
  await expect(page).toHaveURL(/doc=harvis%2Fdocs%2Freference%2Fbrand-guide.md/);
  await expect(pane.getByRole("heading", { name: "Quiet" })).toBeVisible();
  await expect(tree).toBeVisible();
  const docFacts = pane.locator("dl[aria-label='Doc details']");
  await expect(docFacts).toContainText("c".repeat(40));
  await expect(docFacts).toContainText("label:harvis");
  await expect(docFacts).toContainText("120 B");
  await expect(docFacts).toContainText("2 · first");
  const docFull = pane.getByRole("link", { name: "Open full" });
  await expect(docFull).toHaveAttribute("target", "_blank");
  const href = (await docFull.getAttribute("href"))!;
  expect(href).toBe(
    "/doc?path=harvis%2Fdocs%2Freference%2Fbrand-guide.md&server=http%3A%2F%2Faxe.test",
  );
  // A linked task opens from its blob.
  await page.getByRole("tab", { name: "blobs" }).click();
  await page.getByLabel("Search blobs").fill("");
  await blobsTable.getByRole("button", { name: "#3" }).click();
  await expect(page).toHaveURL(/task=3/);
  await expect(page.getByLabel("Task detail", { exact: true })).toBeVisible();
  // Selecting a store item replaces the task in the pane.
  await blobsTable.getByRole("button", { name: "shot.png" }).click();
  await expect(page).not.toHaveURL(/task=3/);
  await expect(pane.getByRole("img", { name: "Preview of shot.png" })).toBeVisible();
  await blobsTable.getByRole("button", { name: "#3" }).click();
  await expect(page.getByLabel("Task detail", { exact: true })).toBeVisible();
  // Escape closes the task, then the store item under it.
  await page.keyboard.press("Escape");
  await expect(pane.getByRole("img", { name: "Preview of shot.png" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.locator("[data-store-viewer]")).toHaveCount(0);
  // The doc's full page renders it on its own.
  await page.goto(href);
  await expect(page.getByRole("heading", { name: "Quiet" })).toBeVisible();
  await expect(page.getByText("harvis/docs/reference/brand-guide.md")).toBeVisible();
});

test("a store item fills the pane height", async ({ page }) => {
  await mockDesk(page);
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 1000 });
  for (const item of [`blob=${sha("3")}`, "kind=docs&doc=harvis/docs/reference/brand-guide.md"]) {
    await page.goto(`/?server=http://axe.test&view=store&${item}`);
    const pane = page.getByRole("region", { name: "Store item" });
    const viewer = pane.locator("[data-store-viewer]");
    await expect(viewer).toBeVisible();
    // Short content: the viewer reaches the pane's bottom (no blank band).
    const [p, v] = await Promise.all([pane.boundingBox(), viewer.boundingBox()]);
    expect(p!.y + p!.height - (v!.y + v!.height)).toBeLessThan(40);
  }
});

test("store rows stay on one line and docs browse as folders", async ({ page }) => {
  await mockDesk(page);
  await mockStore(page, true);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/?server=http://axe.test&view=store&blob=${sha("5")}`);
  const blobsTable = page.getByRole("table", { name: "Blobs" });
  await expect(page.getByRole("region", { name: "Store item" })).toBeVisible();
  // With the pane open, every cell keeps one line: the short date, the full
  // timestamp on hover.
  const row = blobsTable.locator("tbody tr").first();
  // evaluateAll does not wait: let the blob rows render first.
  await expect(row).toBeVisible();
  const heights = await row
    .locator("td")
    .evaluateAll((cells) =>
      cells
        // A narrow list hides the Type column.
        .filter((c) => c.getClientRects().length)
        .map((c) => Math.round(c.getBoundingClientRect().height)),
    );
  expect(new Set(heights).size).toBe(1);
  const stored = row.locator("td").nth(3);
  await expect(stored).not.toContainText("2026");
  await expect(stored).toHaveAttribute("title", /2026/);
  await expect(row.locator("td").nth(1)).toHaveAttribute("title", "text/tab-separated-values");

  // Docs: top-level folders open, deeper ones collapsed until toggled.
  await page.goto("/?server=http://axe.test&view=store&kind=docs");
  const tree = page.getByRole("list", { name: "Docs" });
  const docs = tree.getByRole("button", { name: "docs/" });
  await expect(docs).toHaveAttribute("aria-expanded", "false");
  await expect(tree.getByRole("button", { name: "brand-guide.md" })).toHaveCount(0);
  await docs.click();
  await tree.getByRole("button", { name: "reference/" }).click();
  await tree.getByRole("button", { name: "brand-guide.md" }).click();
  await expect(page.getByRole("region", { name: "Store item" }).getByRole("heading", { name: "Quiet" })).toBeVisible();
  await tree.getByRole("button", { name: "reference/" }).click();
  await expect(tree.getByRole("button", { name: "brand-guide.md" })).toHaveCount(0);
  // A deep link opens the folders above the selected doc.
  await page.goto("/?server=http://axe.test&view=store&kind=docs&doc=harvis/docs/reference/brand-guide.md");
  await expect(tree.getByRole("button", { name: "brand-guide.md" })).toBeVisible();
});

test("the doc page scrolls with the window to the doc's end", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto(`/doc?server=http://axe.test&path=${longDocPath}`);
  const end = page.getByText("The end of the long report.");
  await expect(end).toBeAttached();
  // The window scrolls, not a capped box inside the page.
  const sizes = await page.evaluate(() => ({
    sh: document.scrollingElement!.scrollHeight,
    sw: document.scrollingElement!.scrollWidth,
    w: innerWidth,
    h: innerHeight,
  }));
  expect(sizes.sh).toBeGreaterThan(sizes.h * 2);
  // The wide code block scrolls inside itself; the page never scrolls sideways.
  expect(sizes.sw).toBeLessThanOrEqual(sizes.w);
  const pre = page.locator("pre").filter({ hasText: "wide xxx" });
  expect(await pre.evaluate((e) => e.scrollWidth > e.clientWidth)).toBe(true);
  await page.evaluate(() => window.scrollTo(0, document.scrollingElement!.scrollHeight));
  await expect(end).toBeInViewport();
  // The version line stays; frontmatter is a facts block, not a paragraph.
  await expect(page.getByText(/doc · version cccccccccccc/)).toBeVisible();
  const facts = page.getByLabel("Frontmatter");
  await page.evaluate(() => window.scrollTo(0, 0));
  await expect(facts).toContainText("Long report");
  await expect(facts).toContainText("draft");
  await expect(page.getByText("title:")).toHaveCount(0);
});

test("an open store item narrows the store column instead of covering it", async ({ page }) => {
  await mockDesk(page);
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  // A saved detail width wider than the room beside the store's list.
  await page.goto("/?server=http://axe.test&view=store");
  await page.evaluate(() => localStorage.setItem("axe-layout", JSON.stringify({ detail: 1000 })));
  for (const item of [`blob=${sha("5")}`, `kind=docs&doc=${longDocPath}`]) {
    await page.goto(`/?server=http://axe.test&view=store&${item}`);
    await expect(page.getByRole("region", { name: "Store item" }).locator("[data-store-viewer]")).toBeVisible();
    const list = page.getByRole("region", { name: "Task workspace" });
    const box = (await list.boundingBox())!;
    expect(await list.evaluate((e) => e.scrollWidth <= e.clientWidth)).toBe(true);
    const parts = [
      page.getByRole("region", { name: "Store stats" }).locator(":scope > *"),
      page.getByRole("searchbox"),
    ];
    for (const part of parts)
      for (const b of await part.evaluateAll((els) => els.map((e) => e.getBoundingClientRect().toJSON())))
        expect(b.right).toBeLessThanOrEqual(box.x + box.width);
  }
});
