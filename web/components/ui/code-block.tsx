import type { ComponentProps } from "react";
import { cx } from "./cx";
import styles from "./code-block.module.css";
export function CodeBlock({ className, ...props }: ComponentProps<"pre">) {
  return <pre className={cx(styles.code, className)} {...props} />;
}
// Unified diff with addition/deletion colors from the brand accents.
export function DiffBlock({ body }: { body: string }) {
  return (
    <CodeBlock data-diff="">
      {body.split("\n").map((line, i) => (
        <span
          key={i}
          className={
            /^(\+\+\+|---|@@)/.test(line)
              ? styles.meta
              : line.startsWith("+")
                ? styles.add
                : line.startsWith("-")
                  ? styles.remove
                  : undefined
          }
        >
          {line}
          {"\n"}
        </span>
      ))}
    </CodeBlock>
  );
}
