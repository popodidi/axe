import type { ReactNode } from "react";
import styles from "./prose.module.css";
import { cx } from "./cx";
// compact: tighter type and spacing for the detail side pane.
export function Prose({
  children,
  compact = false,
}: {
  children: ReactNode;
  compact?: boolean;
}) {
  return (
    <div className={cx(styles.prose, compact && styles.compact)}>
      {children}
    </div>
  );
}
