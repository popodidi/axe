import type { ComponentProps } from "react";
import { cx } from "./cx";
import styles from "./table.module.css";
export function Table({ className, ...props }: ComponentProps<"table">) {
  return (
    <div className={styles.scroll}>
      <table className={cx(styles.table, className)} {...props} />
    </div>
  );
}
type CellProps = {
  numeric?: boolean;
  mono?: boolean;
  primary?: boolean;
  zero?: boolean;
  // One line with an ellipsis, taking the remaining row width. Keeps rows at
  // the compact height with long titles; pass the full text as `title`.
  truncate?: boolean;
};
function cellClass(
  { numeric, mono, primary, zero, truncate }: CellProps,
  className?: string,
) {
  return (
    cx(
      numeric && styles.num,
      mono && styles.mono,
      primary && styles.primary,
      zero && styles.zero,
      truncate && styles.truncate,
      className,
    ) || undefined
  );
}
export function Th({
  numeric,
  mono,
  className,
  ...props
}: ComponentProps<"th"> & CellProps) {
  return <th className={cellClass({ numeric, mono }, className)} {...props} />;
}
export function Td({
  numeric,
  mono,
  primary,
  zero,
  truncate,
  className,
  ...props
}: ComponentProps<"td"> & CellProps) {
  return (
    <td
      className={cellClass({ numeric, mono, primary, zero, truncate }, className)}
      {...props}
    />
  );
}
