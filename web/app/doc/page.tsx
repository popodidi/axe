"use client";
// A doc on its own page (the store pane's "Open full"): `/doc?path=<path>`,
// with `&server=` for a standalone build pointed at another axe server. The
// doc shows whole, with metadata and contents rails, and scrolls with the
// window (components/dashboard/doc-page.tsx). The theme toggle is the
// portal's (components/ui/theme-toggle.tsx), on the same saved choice.
import { Suspense } from "react";
import { useSearchParams } from "next/navigation";
import { DocPage } from "../../components/dashboard/doc-page";
import styles from "../../components/dashboard/doc-page.module.css";
import { Alert, Skeleton, ThemeToggle } from "../../components/ui";

function DocRoute() {
  const p = useSearchParams();
  const path = p.get("path") || "";
  const base = (
    p.get("server") || (typeof location === "undefined" ? "" : location.origin)
  ).replace(/\/+$/, "");
  return (
    <main className={styles.page} data-doc-page>
      {/* The portal's day/night toggle, out of the way at the top right. */}
      <div className={styles.bar}>
        <ThemeToggle />
      </div>
      {path ? <DocPage base={base} path={path} /> : <Alert>No doc path given.</Alert>}
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
