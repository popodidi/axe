"use client";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { Button } from "./button";
import styles from "./command.module.css";
export function Commands({ children }: { children: ReactNode }) {
  return <div className={styles.commands}>{children}</div>;
}
// Resolves true only after the clipboard accepted the text.
export type CopyHandler = (command: string) => Promise<boolean> | boolean | void;
const resetAfter = 1500;
// Stable-width Copy button: "Copied" with a check appears only after clipboard
// success and resets after 1.5s; a failure stays actionable as "Copy".
export function CopyButton({
  text,
  label,
  onCopy,
}: {
  text: string;
  label: string;
  onCopy: CopyHandler;
}) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  return (
    <Button
      aria-label={label}
      title={text}
      onClick={async () => {
        const ok = await onCopy(text);
        clearTimeout(timer.current);
        if (ok === false) return setCopied(false);
        setCopied(true);
        timer.current = setTimeout(() => setCopied(false), resetAfter);
      }}
    >
      <span className={styles.copy}>
        <span data-hidden={copied}>Copy</span>
        <span data-hidden={!copied} aria-hidden={!copied}>
          <svg aria-hidden="true" viewBox="0 0 16 16" className={styles.check}>
            <path d="M3 8.5l3 3 7-7" />
          </svg>
          Copied
        </span>
      </span>
    </Button>
  );
}
// One mono shell command with its copy button.
export function Command({
  label,
  command,
  copyLabel,
  onCopy,
}: {
  label: string;
  command: string;
  copyLabel: string;
  onCopy: CopyHandler;
}) {
  return (
    <div className={styles.command}>
      <span className={styles.label}>{label}</span>
      <code className={styles.code}>{command}</code>
      <CopyButton text={command} label={copyLabel} onCopy={onCopy} />
    </div>
  );
}
