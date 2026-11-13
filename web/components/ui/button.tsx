import type { ComponentProps } from "react";
import { cx } from "./cx";
import styles from "./button.module.css";
type Props = ComponentProps<"button"> & {
  variant?: "primary" | "secondary" | "ghost" | "link" | "nav";
  // Compact by default (docs/design/dashboard.md "Compact defaults"); md is opt-in for a
  // prominent standalone action. Link and nav variants size themselves.
  size?: "md" | "sm";
  // Small icon-only control: optional .98 press feedback (brand guide).
  icon?: boolean;
};
export function Button({
  variant = "secondary",
  size = "sm",
  icon = false,
  className,
  type = "button",
  ...props
}: Props) {
  return (
    <button
      type={type}
      className={cx(
        styles.button,
        styles[variant],
        size === "sm" && variant !== "link" && variant !== "nav" && styles.sm,
        icon && styles.icon,
        className,
      )}
      {...props}
    />
  );
}
