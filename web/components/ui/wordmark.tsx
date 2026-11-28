import { cx } from "./cx";
import styles from "./wordmark.module.css";
// Axe OS wordmark: "Axe" 600 in the primary text colour, a word space, then
// "OS" 400 in --wordmark-secondary; Inter, tracking -0.045em. Live text sized
// by the parent font size.
export function Wordmark({ className }: { className?: string }) {
  return (
    <span className={cx(styles.wordmark, className)}>
      <span className={styles.name}>Axe</span> <span className={styles.product}>OS</span>
    </span>
  );
}
