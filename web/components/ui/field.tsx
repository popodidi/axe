import type { ComponentProps } from "react";
import { cx } from "./cx";
import styles from "./field.module.css";
import { Select, type Option } from "./select";
type Compact = { compact?: boolean };
export function Input({
  compact,
  className,
  ...props
}: ComponentProps<"input"> & Compact) {
  return (
    <input
      className={cx(styles.field, compact && styles.compact, className)}
      {...props}
    />
  );
}
export function Search({
  compact,
  className,
  ...props
}: ComponentProps<"input"> & Compact) {
  return (
    <input
      type="search"
      className={cx(
        styles.field,
        styles.search,
        compact && styles.compact,
        className,
      )}
      {...props}
    />
  );
}
// Selects are the styled listbox in ./select; Filter is a captioned Select.
export { Select };
export function Filter({
  label,
  "aria-label": ariaLabel,
  value,
  options,
  onChange,
  className,
}: {
  label: string;
  "aria-label"?: string;
  value: string;
  options: Option[];
  onChange: (value: string) => void;
  className?: string;
}) {
  return (
    <Select
      label={label}
      ariaLabel={ariaLabel}
      value={value}
      options={options}
      onChange={onChange}
      className={className}
    />
  );
}
