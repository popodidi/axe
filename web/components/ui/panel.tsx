import type { ComponentProps, ReactNode } from "react";
import { cx } from "./cx";
import { SectionLabel } from "./section-label";
import styles from "./panel.module.css";
// A layout section, not a visible box (Quiet: group with space first).
export function Panel({ className, ...props }: ComponentProps<"section">) {
  return <section className={cx(styles.panel, className)} {...props} />;
}
export function PanelHead({
  label,
  meta,
  children,
}: {
  label: ReactNode;
  meta?: ReactNode;
  children?: ReactNode;
}) {
  return (
    <div className={styles.head}>
      <SectionLabel as="h3">{label}</SectionLabel>
      {meta !== undefined && <div className={styles.meta}>{meta}</div>}
      {children}
    </div>
  );
}
