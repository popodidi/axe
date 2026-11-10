import type { ReactNode } from "react";
import { cx } from "./cx";
import styles from "./section-label.module.css";
// Sentence-case label; `title` renders a Quiet section title (18/24 600).
export function SectionLabel({
  children,
  as: Tag = "div",
  title = false,
  id,
}: {
  children: ReactNode;
  as?: "div" | "h2" | "h3" | "span";
  title?: boolean;
  id?: string;
}) {
  return (
    <Tag id={id} className={cx(styles.label, title && styles.title)}>
      {children}
    </Tag>
  );
}
