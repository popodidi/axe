import type { ReactNode } from "react";
import styles from "./prose.module.css";
import { cx } from "./cx";
// compact: tighter type and spacing for the detail side pane.
// document: a doc on its own page (app/doc), set for reading.
export function Prose({
  children,
  compact = false,
  document = false,
}: {
  children: ReactNode;
  compact?: boolean;
  document?: boolean;
}) {
  return (
    <div className={cx(styles.prose, compact && styles.compact, document && styles.document)}>
      {children}
    </div>
  );
}
