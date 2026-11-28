import { test, expect, type Locator, type Page, type Route } from "@playwright/test";
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
  // An intro and five sections of ten paragraphs, for the contents rail;
  // section 2 has a subsection.
  ...Array.from({ length: 60 }, (_, i) => [
    ...(i && i % 10 === 0 ? [`## Section ${i / 10}`] : []),
    ...(i === 25 ? ["### Section 2 detail"] : []),
    `Paragraph ${i + 1} of the long report.`,
  ]).flat(),
  "```",
  `wide ${"x".repeat(400)}`,
  ...Array.from({ length: 40 }, (_, i) => `line ${i + 1}`),
  "```",
  "The end of the long report.",
].join("\n\n");

// One stored doc of each kind the doc page renders.
const typedDocs: Record<string, string> = {
  "research/report.html": [
    "<!doctype html><html><head><title>Report</title></head><body>",
    "<h1>HTML report</h1><p>See <a href='https://example.com/'>out</a>",
    " and <a href='notes.html'>notes</a>.</p><img src='chart.png' alt='chart'>",
    "<script>document.body.dataset.ran = '1';",
    "try { parent.document.title = 'escaped'; } catch (e) {}",
    "try { parent.localStorage.setItem('leak', '1'); } catch (e) {}",
    "try { document.body.dataset.cookie = document.cookie || 'none'; } catch (e) {}</script>",
    "<img src='x' onerror=\"document.body.dataset.ran = 'onerror'\">",
    "</body></html>",
  ].join(""),
  // A long HTML doc: a title, five sections and a subsection, with the same
  // attempts to run code; and one that brings its own contents.
  "research/long.html": [
    "<!doctype html><html><head><title>Long</title></head><body>",
    "<h1>Long HTML</h1>",
    ...Array.from({ length: 5 }, (_, i) => [
      `<h2>Part ${i + 1}</h2>`,
      i === 1 ? "<h3 id='part-2-detail'>Part 2 detail</h3>" : "",
      ...Array.from({ length: 12 }, (_, j) => `<p>Part ${i + 1} paragraph ${j + 1}.</p>`),
    ]).flat(),
    "<script>document.body.dataset.ran = '1';",
    "try { parent.document.title = 'escaped'; } catch (e) {}",
    "try { document.body.dataset.cookie = document.cookie || 'none'; } catch (e) {}</script>",
    "<img src='x' onerror=\"document.body.dataset.ran = 'onerror'\">",
    "</body></html>",
  ].join(""),
  "research/own-toc.html": [
    "<nav class='toc'><a href='#a'>A</a><a href='#b'>B</a><a href='#c'>C</a><a href='#d'>D</a></nav>",
    "<h2 id='a'>A</h2><h2 id='b'>B</h2><h2 id='c'>C</h2><h2 id='d'>D</h2>",
  ].join(""),
  "ops/config.json": '{"name":"axe","replicas":2,"tags":["a","b"]}',
  "ops/deploy.yaml": "kind: Deployment\nspec:\n  replicas: 2 # two\n",
  "src/main.rs": 'fn main() {\n    let n = 1;\n    println!("{n}");\n}\n',
  "img/dot.png": PNG.toString("base64"),
  "img/mark.svg":
    "<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><rect width='8' height='8'/><script>parent.document.title='svg'</script></svg>",
  "data/rows.csv": 'name,note\nalpha,"one, two"\nbeta,plain\n',
  "notes/readme.weird": "Plain words\n  keep their indent.",
  "bin/blob.dat": "\u0000\u0001\ufffd\ufffd\u0000",
};

type Store = {
  read: boolean;
  tokens: string[];
  signed: string[];
  diffs: string[];
};

// The store's diff between the two logged versions, as git prints it; one
// added line is long enough to wrap on a phone.
const versionDiff = [
  "diff --git a/harvis/docs/reference/brand-guide.md b/harvis/docs/reference/brand-guide.md",
  "index 1111111..2222222 100644",
  "--- a/harvis/docs/reference/brand-guide.md",
  "+++ b/harvis/docs/reference/brand-guide.md",
  "@@ -1,3 +1,4 @@",
  " # Quiet",
  " ",
  "-Group first.",
  "+Group with **space** first.",
  `+${"A long added sentence that keeps going ".repeat(6).trim()}`,
  "",
].join("\n");

async function mockStore(page: Page, read: boolean) {
  const store: Store = { read, tokens: [], signed: [], diffs: [] };
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
      if (p === "/api/docs/diff") {
        store.diffs.push(`${url.searchParams.get("from")}..${url.searchParams.get("to")}`);
        return json({
          path: url.searchParams.get("path"),
          from: url.searchParams.get("from"),
          to: url.searchParams.get("to"),
          diff: url.searchParams.get("from") === url.searchParams.get("to") ? "" : versionDiff,
        });
      }
      if (p === "/api/docs/file" && url.searchParams.get("version") === "e".repeat(40))
        return json({
          path: url.searchParams.get("path"),
          version: "e".repeat(40),
          kind: "doc",
          content: "# Quiet\n\nGroup first.",
        });
      if (p === "/api/docs/file")
        return json({
          path: url.searchParams.get("path"),
          version: "c".repeat(40),
          kind: "doc",
          content:
            url.searchParams.get("path") === longDocPath
              ? longDoc
              : typedDocs[url.searchParams.get("path") || ""] ?? "# Quiet\n\nGroup with **space** first.",
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
    await page.screenshot({ path: test.info().outputPath(`store-item-${item.startsWith("blob") ? "blob" : "doc"}.png`) });
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

test("the doc page renders code, images, tables and falls back to text", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  const open = async (path: string, kind: string) => {
    await page.goto(`/doc?server=http://axe.test&path=${encodeURIComponent(path)}`);
    const body = page.locator(`[data-doc-kind='${kind}']`);
    await expect(body).toBeVisible();
    // The metadata rail for every kind; contents only for markdown.
    await expect(page.getByLabel("Doc details")).toBeVisible();
    await expect(page.getByRole("navigation", { name: "Contents" })).toHaveCount(0);
    return body;
  };
  // JSON pretty-printed and highlighted, one numbered line per line.
  let body = await open("ops/config.json", "code");
  await expect(body.locator("pre")).toHaveAttribute("data-language", "json");
  await expect(body.locator("[class*='line']")).toHaveCount(8);
  await expect(body.locator(".hljs-attr").first()).toHaveText('"name"');
  body = await open("ops/deploy.yaml", "code");
  await expect(body.locator(".hljs-comment")).toHaveText("# two");
  body = await open("src/main.rs", "code");
  await expect(body.locator(".hljs-keyword").first()).toHaveText("fn");
  await expect(body.locator("[class*='line']")).toHaveCount(4);
  // The gutter numbers stay out of the copied text.
  expect(await body.locator("code").innerText()).toContain("fn main() {");
  body = await open("img/dot.png", "image");
  const img = body.getByRole("img", { name: "img/dot.png" });
  await expect.poll(() => img.evaluate((e: HTMLImageElement) => e.naturalWidth)).toBe(1);
  body = await open("img/mark.svg", "image");
  await expect.poll(() => body.locator("img").evaluate((e: HTMLImageElement) => e.naturalWidth)).toBe(8);
  expect(await page.title()).not.toBe("svg");
  body = await open("data/rows.csv", "table");
  await expect(body.getByRole("columnheader")).toHaveText(["name", "note"]);
  await expect(body.getByRole("row").nth(1).getByRole("cell")).toHaveText(["alpha", "one, two"]);
  body = await open("notes/readme.weird", "text");
  await expect(body.locator("pre")).toHaveText("Plain words\n  keep their indent.");
  body = await open("bin/blob.dat", "binary");
  await expect(body.getByRole("link", { name: "Download" })).toHaveAttribute("download", "blob.dat");
});

test("a stored HTML doc renders sandboxed and cannot reach the app", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/doc?server=http://axe.test");
  await page.evaluate(() => {
    document.cookie = "axe_session=app-secret; path=/";
    localStorage.setItem("app", "secret");
  });
  await page.goto("/doc?server=http://axe.test&path=research%2Freport.html");
  const frame = page.locator("iframe[title='Rendered research/report.html']");
  await expect(frame).toBeVisible();
  const sandbox = (await frame.getAttribute("sandbox"))!.split(/\s+/);
  expect(sandbox).not.toContain("allow-scripts");
  expect(sandbox).not.toContain("allow-same-origin");
  const inner = page.frameLocator("iframe[title='Rendered research/report.html']");
  await expect(inner.getByRole("heading", { name: "HTML report" })).toBeVisible();
  // No script ran: not inline, not an event handler.
  await expect(inner.locator("body")).not.toHaveAttribute("data-ran", /.*/);
  await expect(inner.locator("body")).not.toHaveAttribute("data-cookie", /.*/);
  expect(await page.title()).not.toBe("escaped");
  expect(await page.evaluate(() => localStorage.getItem("leak"))).toBeNull();
  // The frame has an opaque origin: no app cookies or storage inside it.
  const origin = await frame.evaluate((f: HTMLIFrameElement) => {
    try {
      return f.contentWindow!.document.cookie;
    } catch {
      return "blocked";
    }
  });
  expect(origin).toBe("blocked");
  // Absolute links open in a new tab; relative ones and images degrade.
  await expect(inner.getByRole("link", { name: "out" })).toHaveAttribute("href", "https://example.com/");
  await expect(inner.getByText("notes", { exact: true })).not.toHaveAttribute("href", /.*/);
  await expect(inner.getByText("[chart]")).toBeVisible();
  // The Source toggle shows the stored markup, script and all, as code.
  await expect(page.getByRole("navigation", { name: "Contents" })).toHaveCount(0);
  await page.getByRole("tab", { name: "Source" }).click();
  await expect(frame).toHaveCount(0);
  await expect(page.locator("[data-doc-kind='html'] pre")).toContainText("<script>document.body.dataset.ran");
  await page.getByRole("tab", { name: "Rendered" }).click();
  await expect(frame).toBeVisible();
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

test("the doc page shows its metadata and contents beside the body", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  // The store's metadata for a listed doc.
  await page.goto("/doc?server=http://axe.test&path=harvis/docs/reference/brand-guide.md");
  const details = page.getByLabel("Doc details");
  await expect(details).toContainText("120 B");
  await expect(details).toContainText("label:harvis");
  await expect(details).toContainText("2 · first");
  // A long value stays on one line, whole in its tooltip.
  const hash = details.getByTitle("c".repeat(40));
  await expect(hash).toHaveText("c".repeat(40));
  expect(await hash.evaluate((e) => e.scrollWidth > e.clientWidth)).toBe(true);

  await page.goto(`/doc?server=http://axe.test&path=${longDocPath}`);
  const meta = page.getByRole("complementary", { name: "Doc metadata" });
  const body = page.locator("article");
  const toc = page.getByRole("navigation", { name: "Contents" });
  // Five sections and a subsection, numbered; the title is not listed.
  await expect(toc.getByRole("link")).toHaveCount(6);
  await expect(toc.getByRole("link")).toHaveText([
    "1.Section 1",
    "2.Section 2",
    "2.1Section 2 detail",
    "3.Section 3",
    "4.Section 4",
    "5.Section 5",
  ]);
  // Collapsed by default: a slim strip of ticks at the left of the rail's
  // column, one per heading, shorter by level, the current section's in
  // primary text. The rail itself keeps its open width (a fixed rail).
  const rail = toc.locator("..");
  const pin = rail.getByRole("button", { name: "Pin contents open" });
  await expect(pin).toHaveAttribute("aria-pressed", "false");
  await expect(page.getByText("Contents", { exact: true }).first()).toBeHidden();
  const railCollapsed = (await rail.boundingBox())!;
  expect(railCollapsed.width).toBeGreaterThan(200);
  const collapsed = (await toc.boundingBox())!;
  expect(collapsed.width).toBeLessThanOrEqual(32);
  expect(collapsed.x).toBe(railCollapsed.x);
  const ticks = toc.locator("a > span:first-child");
  await expect(ticks).toHaveCount(6);
  const tickWidth = (i: number) => ticks.nth(i).evaluate((e) => e.getBoundingClientRect().width);
  expect(await tickWidth(2)).toBeLessThan(await tickWidth(1));
  const tickColor = (i: number) => ticks.nth(i).evaluate((e) => getComputedStyle(e).backgroundColor);
  const primary = await page.evaluate(() => {
    const probe = document.createElement("div");
    probe.style.color = "var(--text-primary)";
    document.body.append(probe);
    const c = getComputedStyle(probe).color;
    probe.remove();
    return c;
  });
  await expect.poll(() => tickColor(0)).toBe(primary);
  expect(await tickColor(1)).not.toBe(primary);
  const bodyCollapsed = (await body.boundingBox())!;
  // A dense strip: the ticks sit 6px apart.
  const centers = () =>
    ticks.evaluateAll((ts) =>
      ts.map((t) => {
        const r = t.getBoundingClientRect();
        return { y: r.top + r.height / 2, x: r.left, w: r.width, h: r.height };
      }),
    );
  const rest = await centers();
  for (let i = 1; i < rest.length; i++) expect(rest[i].y - rest[i - 1].y).toBeCloseTo(6, 0);
  // The toggle's mark sits on the ticks' left edge, as long as a top-level tick.
  const mark = (await pin.locator("svg").boundingBox())!;
  expect(Math.abs(mark.x - rest[0].x)).toBeLessThanOrEqual(1);
  expect(Math.abs(mark.width - rest[0].w)).toBeLessThanOrEqual(1);
  // Its glyph is a side panel, not stacked lines that read as more ticks.
  await expect(pin.locator("svg rect")).toHaveCount(1);
  await page.screenshot({ path: test.info().outputPath("doc-contents-collapsed.png") });
  // Hovering magnifies like a dock: the tick under the pointer grows longest
  // and thickest, its neighbours less by distance, and the rows around it
  // open; the list does not open and the body does not move.
  const peek = rail.locator("[popover]");
  const names = ["1. Section 1", "2. Section 2", "2.1 Section 2 detail", "3. Section 3", "4. Section 4", "5. Section 5"];
  // Move the pointer down the strip, as a reader does, until the dock names
  // tick `i`; the tick under the pointer is the one magnified.
  const hoverTick = async (i: number) => {
    const top = rest[0].y - 3;
    for (let y = top; y < top + 120; y += 2) {
      await page.mouse.move(rest[0].x + 4, y);
      await page.waitForTimeout(20);
      if ((await peek.textContent()) === names[i] && (await peek.isVisible())) break;
    }
    await page.waitForTimeout(300);
    await expect(peek).toHaveText(names[i]);
  };
  await page.mouse.move(rest[3].x + 4, rest[3].y);
  await expect(peek).toBeVisible();
  await hoverTick(3);
  const zoomed = await centers();
  expect(zoomed[3].w).toBeGreaterThan(rest[3].w * 1.4);
  expect(zoomed[3].h).toBeGreaterThan(rest[3].h * 1.5);
  expect(zoomed[4].w / rest[4].w).toBeLessThan(zoomed[3].w / rest[3].w);
  expect(zoomed[4].w / rest[4].w).toBeGreaterThan(1.1);
  expect(zoomed[2].w / rest[2].w).toBeGreaterThan(1.1);
  expect(zoomed[4].y - zoomed[3].y).toBeGreaterThan(12);
  // The title is level with its tick, just right of the rail.
  const label = (await peek.boundingBox())!;
  expect(Math.abs(label.y + label.height / 2 - zoomed[3].y)).toBeLessThan(2);
  expect(label.x).toBeGreaterThanOrEqual(collapsed.x + collapsed.width);
  await expect(page.getByText("Contents", { exact: true }).first()).toBeHidden();
  expect((await toc.boundingBox())!.width).toBeLessThanOrEqual(32);
  expect((await rail.boundingBox())!.width).toBe(railCollapsed.width);
  expect((await body.boundingBox())!.x).toBe(bodyCollapsed.x);
  // Off the strip, in the rail's empty width, the dock rests.
  await page.mouse.move(700, 600);
  await expect(peek).toBeHidden();
  await page.mouse.move(collapsed.x + 120, rest[3].y);
  await page.waitForTimeout(200);
  await expect(peek).toBeHidden();
  await page.mouse.move(rest[3].x + 4, rest[3].y);
  await expect(peek).toBeVisible();
  await page.screenshot({ path: test.info().outputPath("doc-contents-hover.png") });
  // Moving down follows the pointer to the next tick.
  await hoverTick(4);
  // Leaving eases everything back.
  await page.mouse.move(700, 600);
  await expect(peek).toBeHidden();
  await expect
    .poll(async () => (await centers()).map((c) => [Math.round(c.y), Math.round(c.w)]))
    .toEqual(rest.map((c) => [Math.round(c.y), Math.round(c.w)]));
  // A click on a tick jumps to its section.
  await hoverTick(4);
  await page.mouse.down();
  await page.mouse.up();
  await expect(page).toHaveURL(/#section-4$/);
  await page.mouse.move(700, 600);
  // With reduced motion the hovered tick is only highlighted and named.
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.mouse.move(rest[2].x + 4, rest[2].y);
  await expect(peek).toHaveText("2.1 Section 2 detail");
  expect(await tickColor(2)).toBe(primary);
  expect((await centers()).map((c) => [Math.round(c.y), Math.round(c.w)])).toEqual(
    rest.map((c) => [Math.round(c.y), Math.round(c.w)]),
  );
  await page.mouse.move(700, 600);
  await page.emulateMedia({ reducedMotion: null });
  await page.goto(`/doc?server=http://axe.test&path=${longDocPath}`);
  // The keyboard reaches the rail: focus opens it, Enter on the pin button
  // keeps it open, and the choice survives a reload.
  await pin.focus();
  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Tab");
  await expect(pin).toBeFocused();
  await expect(page.getByText("Contents", { exact: true }).first()).toBeVisible();
  // It opens in place at the rail's one width and the list slides in: no
  // width tween that would lay the labels out too narrow, so no label wraps,
  // and no overlay over the body.
  const opened = await rail.evaluate((e) => ({
    width: e.getBoundingClientRect().width,
    tween: getComputedStyle(e).transitionProperty.includes("width"),
    slides: getComputedStyle(e.querySelector("nav")!).animationName,
    shadow: getComputedStyle(e).boxShadow,
  }));
  expect(opened).toMatchObject({ tween: false, shadow: "none" });
  expect(opened.slides).toMatch(/rail-slide-in/);
  expect(opened.width).toBe(railCollapsed.width);
  expect((await body.boundingBox())!.x).toBe(bodyCollapsed.x);
  const rowHeights = await toc.getByRole("link").evaluateAll((as) => as.map((a) => a.getBoundingClientRect().height));
  expect(new Set(rowHeights).size).toBe(1);
  await page.keyboard.press("Enter");
  const unpin = rail.getByRole("button", { name: "Collapse contents" });
  await expect(unpin).toHaveAttribute("aria-pressed", "true");
  await expect(unpin).toBeFocused();
  expect(await page.evaluate(() => localStorage.getItem("axe-toc-pinned"))).toBe("1");
  await page.goto(`/doc?server=http://axe.test&path=${longDocPath}`);
  await expect(unpin).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByText("Contents", { exact: true }).first()).toBeVisible();
  // Restored on load, the pinned rail does not animate.
  const motion = () =>
    rail.evaluate((e) =>
      Array.from(e.querySelectorAll(":scope > :not([popover])"), (c) => getComputedStyle(c).animationName),
    );
  expect(await motion()).toEqual(["none", "none"]);
  await page.mouse.move(700, 600);
  // Pinned, the rail keeps its width and the body does not move.
  expect((await rail.boundingBox())!.width).toBe(railCollapsed.width);
  expect((await body.boundingBox())!.x).toBe(bodyCollapsed.x);
  await page.screenshot({ path: test.info().outputPath("doc-contents-pinned.png") });
  // Collapsing and pinning by the toggle slide the content (transform and
  // opacity) inside a rail of one width; every frame keeps the rail's width
  // and the body's place, and no label wraps. The list slides out first,
  // the strip back in after it.
  const frames = (n: number) =>
    page.evaluate(
      (n) =>
        new Promise<{ rail: number; body: number; moving: boolean; wraps: number; leaving: boolean }[]>((done) => {
          const rail = document.querySelector<HTMLElement>("[data-toc-rail]")!;
          const body = document.querySelector("article")!;
          const out: { rail: number; body: number; moving: boolean; wraps: number; leaving: boolean }[] = [];
          const step = () => {
            const nav = rail.querySelector("nav")!;
            out.push({
              rail: rail.getBoundingClientRect().width,
              body: body.getBoundingClientRect().x,
              moving: getComputedStyle(nav).transform !== "none" && getComputedStyle(nav).transform !== "matrix(1, 0, 0, 1, 0, 0)",
              wraps: rail.hasAttribute("data-pinned")
                ? Array.from(nav.querySelectorAll("a .text")).filter((t) => t.getClientRects().length > 1).length
                : 0,
              leaving: rail.hasAttribute("data-leaving"),
            });
            if (out.length < n) requestAnimationFrame(step);
            else done(out);
          };
          requestAnimationFrame(step);
        }),
      n,
    );
  const toggle = async () => {
    const f = frames(40);
    await rail.getByRole("button").click();
    return f;
  };
  const steady = (fs: Awaited<ReturnType<typeof frames>>) => {
    expect(new Set(fs.map((f) => f.rail))).toEqual(new Set([railCollapsed.width]));
    expect(new Set(fs.map((f) => f.body))).toEqual(new Set([bodyCollapsed.x]));
    expect(fs.every((f) => f.wraps === 0)).toBe(true);
    expect(fs.some((f) => f.moving)).toBe(true);
  };
  const out = await toggle();
  steady(out);
  expect(out.some((f) => f.leaving)).toBe(true);
  await expect(rail).not.toHaveAttribute("data-pinned");
  await expect(page.getByText("Contents", { exact: true }).first()).toBeHidden();
  steady(await toggle());
  await expect(rail).toHaveAttribute("data-pinned");
  // A second press while it slides out reverses it: still pinned.
  await rail.getByRole("button").click();
  await expect(rail).toHaveAttribute("data-leaving");
  await rail.getByRole("button").click();
  await expect(rail).not.toHaveAttribute("data-leaving");
  await expect(rail.getByRole("button")).toHaveAttribute("aria-pressed", "true");
  // With reduced motion they swap at once, with nothing moving.
  await page.emulateMedia({ reducedMotion: "reduce" });
  const still = await toggle();
  expect(still.every((f) => !f.moving && !f.leaving)).toBe(true);
  await expect(rail).not.toHaveAttribute("data-pinned");
  await rail.getByRole("button").click();
  await expect(rail).toHaveAttribute("data-pinned");
  await page.emulateMedia({ reducedMotion: null });
  await page.mouse.move(700, 600);
  // The title outranks the sections and the sections the text; a section
  // sits closer to its text than to the text before it.
  const size = (l: Locator) => l.evaluate((e) => parseFloat(getComputedStyle(e).fontSize));
  const box = async (l: Locator) => (await l.boundingBox())!;
  const para = (n: number) => body.getByText(`Paragraph ${n} of the long report.`, { exact: true });
  const h2 = body.getByRole("heading", { name: "Section 1" });
  const h1Size = await size(body.getByRole("heading", { name: "Long report" }));
  expect(h1Size).toBeGreaterThan(await size(h2));
  expect(await size(h2)).toBeGreaterThan(await size(para(1)));
  const [before, head, after] = await Promise.all([box(para(10)), box(h2), box(para(11))]);
  expect(head.y - (before.y + before.height)).toBeGreaterThan(after.y - (head.y + head.height));
  // Text keeps a reading measure; code uses the whole column.
  const [text, code] = await Promise.all([box(para(1)), box(body.locator("pre"))]);
  expect(text.width).toBeLessThan(code.width);
  // The measure is 76ch to a 1200px window, then a little wider, at most
  // 83ch: about 4% more here at 1440px, more on a large screen.
  const ch = await para(1).evaluate((e) => {
    const probe = document.createElement("span");
    probe.style.cssText = "display:inline-block;width:1ch";
    e.append(probe);
    const w = probe.getBoundingClientRect().width;
    probe.remove();
    return w;
  });
  expect(text.width / ch).toBeCloseTo(76 + 240 / 10 / ch, 0);
  await page.setViewportSize({ width: 1920, height: 900 });
  expect((await box(para(1))).width / ch).toBeCloseTo(83, 0);
  await page.setViewportSize({ width: 1440, height: 900 });
  // The rail shows; the narrow layout's fold does not.
  await expect(page.locator("summary", { hasText: "Contents" })).toBeHidden();
  // Contents left, body center, metadata right, on one row.
  const [m, b, t] = await Promise.all([meta.boundingBox(), body.boundingBox(), toc.boundingBox()]);
  expect(t!.x + t!.width).toBeLessThan(b!.x);
  expect(b!.x + b!.width).toBeLessThan(m!.x);
  expect(Math.abs(m!.y - b!.y)).toBeLessThan(4);
  // The body is wider than the old single column (840px).
  expect(b!.width).toBeGreaterThan(860);
  // The text's measure is centred in the body: as far from the contents
  // rail as from the metadata, the headings on the text's left edge.
  const railBox = (await rail.boundingBox())!;
  const left = text.x - (railBox.x + railBox.width);
  const right = m!.x - (text.x + text.width);
  expect(Math.abs(left - right)).toBeLessThanOrEqual(2);
  expect(left).toBeGreaterThan(100);
  expect(Math.abs((await box(h2)).x - text.x)).toBeLessThanOrEqual(1);
  // The first section is current at the top.
  await expect(toc.getByRole("link", { name: "Section 1" })).toHaveAttribute("aria-current", "location");
  // Scrolling marks the section in view; the rails stay put.
  await page.getByRole("heading", { name: "Section 3" }).evaluate((e) =>
    window.scrollTo(0, e.getBoundingClientRect().top + scrollY - 40),
  );
  await expect(toc.getByRole("link", { name: "Section 3" })).toHaveAttribute("aria-current", "location");
  await expect(toc.locator("[aria-current]")).toHaveCount(1);
  // The current entry has the selected fill and semibold text; the others
  // are plain.
  const look = (name: string) =>
    toc.getByRole("link", { name, exact: true }).evaluate((e) => {
      const c = getComputedStyle(e);
      return { bg: c.backgroundColor, weight: c.fontWeight };
    });
  const selected = await page.evaluate(() => {
    const probe = document.createElement("div");
    probe.style.background = "var(--surface-selected)";
    document.body.append(probe);
    const bg = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return bg;
  });
  await expect.poll(() => look("Section 3")).toEqual({ bg: selected, weight: "600" });
  expect((await look("Section 1")).weight).not.toBe("600");
  await page.getByRole("heading", { name: "Section 2 detail" }).evaluate((e) =>
    window.scrollTo(0, e.getBoundingClientRect().top + scrollY - 40),
  );
  await expect(toc.getByRole("link", { name: "Section 2 detail" })).toHaveAttribute("aria-current", "location");
  await page.waitForTimeout(300);
  await page.screenshot({ path: test.info().outputPath("doc-contents-mid-scroll.png") });
  // The h3 indents its text past the h2's.
  const pad = (name: string) =>
    toc.getByRole("link", { name, exact: true }).evaluate((e) => parseFloat(getComputedStyle(e).paddingLeft));
  expect(await pad("Section 2 detail")).toBeGreaterThan(await pad("Section 2"));
  // One page scrollbar: the rails fit the window here and hide their thin
  // scrollbar until hovered.
  for (const rail of [meta, toc.locator("..")]) {
    const r = await rail.evaluate((e) => ({
      fits: e.scrollHeight <= e.clientHeight,
      width: getComputedStyle(e).scrollbarWidth,
      color: getComputedStyle(e).scrollbarColor,
    }));
    expect(r).toMatchObject({ fits: true, width: "thin" });
    expect(r.color).toMatch(/rgba\(0, 0, 0, 0\)|transparent/);
  }
  for (const rail of [meta, toc]) {
    await expect(rail).toBeInViewport();
    expect((await rail.boundingBox())!.y).toBeLessThan(120);
  }
  // A click jumps to the section and puts it in the URL.
  await toc.getByRole("link", { name: "Section 5" }).click();
  await expect(page).toHaveURL(/#section-5$/);
  const heading = page.getByRole("heading", { name: "Section 5" });
  await expect(heading).toBeInViewport();
  expect((await heading.boundingBox())!.y).toBeLessThan(120);
  await expect(toc.getByRole("link", { name: "Section 5" })).toHaveAttribute("aria-current", "location");
  // A shared link opens at its section once the doc loads.
  await page.goto(`/doc?server=http://axe.test&path=${longDocPath}#section-4`);
  await expect(page.getByRole("heading", { name: "Section 4" })).toBeInViewport();
  await expect(toc.getByRole("link", { name: "Section 4" })).toHaveAttribute("aria-current", "location");
});

test("the doc page lists versions, opens an older one and shows the changes", async ({ page }) => {
  const store = await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  const path = "harvis/docs/reference/brand-guide.md";
  await page.goto(`/doc?server=http://axe.test&path=${path}`);
  // Newest first: when, who, why; the current one is open.
  const history = page.getByRole("list", { name: "Doc history" });
  const entries = history.getByRole("listitem");
  await expect(entries).toHaveCount(2);
  await expect(entries.nth(0)).toContainText("current");
  await expect(entries.nth(0)).toContainText("axe · ccccccc");
  await expect(entries.nth(0)).toContainText("edit");
  await expect(entries.nth(1)).toContainText("create");
  await expect(entries.nth(0).getByRole("link").first()).toHaveAttribute("aria-current", "page");

  // An older version opens read only, with the way back.
  await entries.nth(1).getByRole("link").first().click();
  await expect(page).toHaveURL(new RegExp(`version=${"e".repeat(40)}`));
  const article = page.locator("article");
  await expect(article.getByText("Group first.")).toBeVisible();
  const notice = page.getByRole("note", { name: "Older version" });
  await expect(notice).toContainText("by axe. Read only.");
  await expect(page.getByLabel("Doc details")).toContainText("e".repeat(40));
  await notice.getByRole("link", { name: "Current version" }).click();
  await expect(article.getByText("Group with space first.")).toBeVisible();
  await expect(notice).toHaveCount(0);

  // The current version's changes: the previous version against it.
  await entries.nth(0).getByRole("link", { name: /^Changes in/ }).click();
  await expect(page).toHaveURL(/diff=1/);
  const changes = page.getByRole("region", { name: "Changes" });
  await expect(changes.getByLabel("2 added, 1 removed")).toBeVisible();
  await expect(changes.locator("del")).toHaveText(["−Group first."]);
  await expect(changes.locator("ins").first()).toHaveText("+Group with **space** first.");
  await expect(changes.getByText("Lines 1–4")).toBeVisible();
  expect(store.diffs.at(-1)).toBe(`${"e".repeat(40)}..${"c".repeat(40)}`);
  await expect(entries.nth(0).getByRole("link", { name: /^Changes in/ })).toHaveAttribute("aria-current", "page");
  // Any two versions: the same one twice has no changes.
  await changes.getByRole("combobox", { name: "From" }).click();
  await page.getByRole("option", { name: /ccccccc/ }).click();
  await expect(page).toHaveURL(new RegExp(`from=${"c".repeat(40)}`));
  await expect(changes).toContainText("No changes between these versions.");
  await page.goBack();
  await expect(changes.getByLabel("2 added, 1 removed")).toBeVisible();

  // On a phone the diff wraps in the column: no sideways scroll.
  await page.setViewportSize({ width: 390, height: 844 });
  const long = changes.locator("ins").nth(1);
  const height = (l: Locator) => l.evaluate((e) => e.getBoundingClientRect().height);
  expect(await height(long)).toBeGreaterThan((await height(changes.locator("del"))) * 1.5);
  const sizes = await page.evaluate(() => ({ sw: document.scrollingElement!.scrollWidth, w: innerWidth }));
  expect(sizes.sw).toBeLessThanOrEqual(sizes.w);
  await changes.getByRole("link", { name: "Back to the current version" }).click();
  await expect(article.getByText("Group with space first.")).toBeVisible();
});

test("a narrow doc page stacks the metadata and folds the contents", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1024, height: 900 });
  await page.goto(`/doc?server=http://axe.test&path=${longDocPath}`);
  const meta = page.getByRole("complementary", { name: "Doc metadata" });
  const body = page.locator("article");
  await expect(meta.getByLabel("Frontmatter")).toContainText("Long report");
  const fold = page.locator("summary", { hasText: "Contents" });
  const [f, m, b] = await Promise.all([fold.boundingBox(), meta.boundingBox(), body.boundingBox()]);
  // Contents first, then the metadata, then the body.
  expect(f!.y + f!.height).toBeLessThanOrEqual(m!.y);
  expect(m!.y + m!.height).toBeLessThanOrEqual(b!.y);
  const sizes = await page.evaluate(() => ({ sw: document.scrollingElement!.scrollWidth, w: innerWidth }));
  expect(sizes.sw).toBeLessThanOrEqual(sizes.w);
  // On a phone the body has matching gutters left and right.
  await page.setViewportSize({ width: 390, height: 844 });
  const para = (await body.getByText("Paragraph 1 of the long report.", { exact: true }).boundingBox())!;
  expect(Math.abs(para.x - (390 - para.x - para.width))).toBeLessThanOrEqual(1);
  await page.setViewportSize({ width: 1024, height: 900 });
  // One contents list, folded until opened.
  const toc = page.getByRole("navigation", { name: "Contents" });
  await expect(toc).toHaveCount(0);
  await page.locator("summary", { hasText: "Contents" }).click();
  await expect(toc).toBeVisible();
  await toc.getByRole("link", { name: "Section 2", exact: true }).click();
  await expect(page).toHaveURL(/#section-2$/);
  await expect(page.getByRole("heading", { name: "Section 2", exact: true })).toBeInViewport();
});

test("a doc that is not markdown has no contents", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/doc?server=http://axe.test&path=notes/plain.txt");
  await expect(page.getByText("# Quiet")).toBeVisible();
  await expect(page.getByText("doc · version cccccccccccc")).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Contents" })).toHaveCount(0);
  await expect(page.getByText("Contents", { exact: true })).toHaveCount(0);
});

test("a long HTML doc gets contents that follow its frame and stay sandboxed", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/doc?server=http://axe.test");
  await page.evaluate(() => {
    document.cookie = "axe_session=app-secret; path=/";
  });
  await page.goto("/doc?server=http://axe.test&path=research%2Flong.html");
  const frame = page.locator("iframe[title='Rendered research/long.html']");
  const inner = page.frameLocator("iframe[title='Rendered research/long.html']");
  const toc = page.getByRole("navigation", { name: "Contents" });
  await expect(toc.getByRole("link")).toHaveText([
    "1.Part 1",
    "2.Part 2",
    "2.1Part 2 detail",
    "3.Part 3",
    "4.Part 4",
    "5.Part 5",
  ]);
  // The frame runs only the page's scroll spy: still an opaque origin, and
  // the doc's own script and handlers never ran.
  const sandbox = (await frame.getAttribute("sandbox"))!.split(/\s+/);
  expect(sandbox).toContain("allow-scripts");
  expect(sandbox).not.toContain("allow-same-origin");
  await expect(inner.getByRole("heading", { name: "Part 1" })).toBeVisible();
  await expect(inner.locator("body")).not.toHaveAttribute("data-ran", /.*/);
  await expect(inner.locator("body")).not.toHaveAttribute("data-cookie", /.*/);
  expect(await page.title()).not.toBe("escaped");
  expect(
    await frame.evaluate((f: HTMLIFrameElement) => {
      try {
        return f.contentWindow!.document.cookie;
      } catch {
        return "blocked";
      }
    }),
  ).toBe("blocked");
  // The section in view follows the frame's scroll.
  await expect(toc.getByRole("link", { name: "Part 1" })).toHaveAttribute("aria-current", "location");
  await inner.getByRole("heading", { name: "Part 3" }).evaluate((e) =>
    window.scrollTo(0, e.getBoundingClientRect().top + scrollY - 20),
  );
  await expect(toc.getByRole("link", { name: "Part 3" })).toHaveAttribute("aria-current", "location");
  await expect(toc.locator("[aria-current]")).toHaveCount(1);
  // A click scrolls the frame to the section.
  await toc.getByRole("link", { name: "Part 5" }).click();
  await expect(inner.getByRole("heading", { name: "Part 5" })).toBeInViewport();
  await expect(toc.getByRole("link", { name: "Part 5" })).toHaveAttribute("aria-current", "location");
  // The Source view has no contents.
  await page.getByRole("tab", { name: "Source" }).click();
  await expect(toc).toHaveCount(0);
  await page.getByRole("tab", { name: "Rendered" }).click();
  await expect(toc.getByRole("link")).toHaveCount(6);
});

test("an HTML doc with its own contents or few headings gets none", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  for (const path of ["research/own-toc.html", "research/report.html"]) {
    await page.goto(`/doc?server=http://axe.test&path=${encodeURIComponent(path)}`);
    const frame = page.locator(`iframe[title='Rendered ${path}']`);
    await expect(frame).toBeVisible();
    await expect(page.getByRole("navigation", { name: "Contents" })).toHaveCount(0);
    expect(await frame.getAttribute("sandbox")).not.toContain("allow-scripts");
  }
});

test("the doc page's theme toggle is the portal's, on the same saved choice", async ({ page }) => {
  await mockStore(page, true);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/doc?server=http://axe.test&path=harvis/docs/reference/brand-guide.md");
  await expect(page.getByLabel("Doc details")).toBeVisible();
  const toggle = page.getByRole("button", { name: "Light theme" });
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
  // Top right, clear of the doc's rails.
  const box = (await toggle.boundingBox())!;
  expect(box.x + box.width).toBeGreaterThan(1440 - 48);
  expect(box.y).toBeLessThan(48);
  const head = (await page.getByLabel("Doc metadata").locator("header").boundingBox())!;
  expect(box.y + box.height).toBeLessThanOrEqual(head.y);
  await toggle.click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  expect(await page.evaluate(() => localStorage.getItem("axe-theme"))).toBe("light");
  // The portal follows, and its toggle switches the doc page back.
  await page.goto("/?server=http://axe.test&view=store");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await page.getByRole("button", { name: "Light theme" }).click();
  await expect(page.locator("html")).not.toHaveAttribute("data-theme", "light");
  await page.goto("/doc?server=http://axe.test&path=harvis/docs/reference/brand-guide.md");
  await expect(page.getByRole("button", { name: "Light theme" })).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator("html")).not.toHaveAttribute("data-theme", "light");
});
