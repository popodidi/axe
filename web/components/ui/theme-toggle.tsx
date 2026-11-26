"use client";
// The day/night theme toggle: one icon button shared by the portal's nav
// footer and the doc page, both on the saved choice in lib/theme.ts, so a
// switch on either page applies to both.
import { useTheme } from "../../lib/theme";
import { Button } from "./button";
import styles from "./theme-toggle.module.css";

export function ThemeToggle({ className }: { className?: string }) {
  const [light, toggle] = useTheme();
  return (
    <Button
      variant="ghost"
      icon
      aria-label="Light theme"
      aria-pressed={light}
      title={light ? "Switch to dark theme" : "Switch to light theme"}
      onClick={toggle}
      className={className}
    >
      <svg aria-hidden viewBox="0 0 16 16" className={styles.icon}>
        <circle cx="8" cy="8" r="5.5" />
        <path d="M8 2.5a5.5 5.5 0 0 1 0 11z" className={styles.fill} />
      </svg>
    </Button>
  );
}
