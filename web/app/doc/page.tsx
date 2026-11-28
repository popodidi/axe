"use client";
// A doc on its own page (the store pane's "Open full"): `/doc?path=<path>`,
// with `&server=` for a standalone build pointed at another axe server. The
// doc shows whole, with metadata and contents rails, and scrolls with the
// window (components/dashboard/doc-page.tsx). The theme toggle is the
// portal's (components/ui/theme-toggle.tsx), on the same saved choice.
// `&version=<v>` opens an older version; `&diff=1` (optional `&from=`,
// `&to=`) shows the changes between two (components/dashboard/doc-history.tsx).
import { Suspense } from "react";
import { useSearchParams } from "next/navigation";
import { DocPage } from "../../components/dashboard/doc-page";
import type { DocView } from "../../components/dashboard/doc-history";
import styles from "../../components/dashboard/doc-page.module.css";
import { Alert, Skeleton, ThemeToggle } from "../../components/ui";

function DocRoute() {
  const p = useSearchParams();
  const path = p.get("path") || "";
  const base = (
    p.get("server") || (typeof location === "undefined" ? "" : location.origin)
  ).replace(/\/+$/, "");
  const view: DocView = {
    version: p.get("version") || undefined,
    diff: p.get("diff")
      ? { from: p.get("from") || undefined, to: p.get("to") || undefined }
      : undefined,
  };
  // The same page for another view of this doc, on the same server.
  const href = (v: DocView) => {
    const q = new URLSearchParams();
    const server = p.get("server");
    if (server) q.set("server", server);
    q.set("path", path);
    if (v.version) q.set("version", v.version);
    if (v.diff) {
      q.set("diff", "1");
      if (v.diff.from) q.set("from", v.diff.from);
      if (v.diff.to) q.set("to", v.diff.to);
    }
    return `/doc?${q}`;
  };
  return (
    <main className={styles.page} data-doc-page>
      {/* The portal's day/night toggle, out of the way at the top right. */}
      <div className={styles.bar}>
        <ThemeToggle />
      </div>
      {path ? <DocPage base={base} path={path} view={view} href={href} /> : <Alert>No doc path given.</Alert>}
    </main>
  );
}

export default function Page() {
  return (
    <Suspense fallback={<Skeleton label="Loading doc…" lines={6} />}>
      <DocRoute />
    </Suspense>
  );
}
