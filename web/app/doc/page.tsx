"use client";
// A doc on its own page (the store pane's "Open full"): `/doc?path=<path>`,
// with `&server=` for a standalone build pointed at another axe server.
import { Suspense } from "react";
import { useSearchParams } from "next/navigation";
import { DocViewer } from "../../components/dashboard/store-viewer";
import styles from "../../components/dashboard/store.module.css";
import { Alert, Skeleton } from "../../components/ui";

function DocPage() {
  const p = useSearchParams();
  const path = p.get("path") || "";
  const base = (
    p.get("server") || (typeof location === "undefined" ? "" : location.origin)
  ).replace(/\/+$/, "");
  return (
    <main className={styles.page}>
      {path ? (
        <DocViewer base={base} path={path} />
      ) : (
        <Alert>No doc path given.</Alert>
      )}
    </main>
  );
}

export default function Page() {
  return (
    <Suspense fallback={<Skeleton label="Loading doc…" lines={6} />}>
      <DocPage />
    </Suspense>
  );
}
