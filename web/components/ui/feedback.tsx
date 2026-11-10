import type { ReactNode } from "react";
import styles from "./feedback.module.css";
export function EmptyState({ children }: { children: ReactNode }) {
  return <p className={styles.empty}>{children}</p>;
}
export function Skeleton({ label, lines = 3 }: { label: string; lines?: number }) {
  return (
    <div className={styles.skeleton} role="status" aria-label={label}>
      {Array.from({ length: lines }, (_, i) => (
        <span key={i} />
      ))}
    </div>
  );
}
export function Alert({ children }: { children: ReactNode }) {
  return (
    <div role="alert" className={styles.alert}>
      {children}
    </div>
  );
}
export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className={styles.kbd}>{children}</kbd>;
}
export function LiveDot({ live }: { live: boolean }) {
  return <span className={styles.dot} data-live={live} aria-hidden />;
}
