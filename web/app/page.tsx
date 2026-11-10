import { Suspense } from "react";
import Dashboard from "./dashboard";
import { Skeleton } from "../components/ui";
export default function Page() {
  return (
    <Suspense fallback={<Skeleton label="Loading workspace…" lines={6} />}>
      <Dashboard />
    </Suspense>
  );
}
