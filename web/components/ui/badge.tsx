import {
  taskStatus,
  taskStatusLabel,
  taskStatusTitle,
  type Task,
} from "../../lib/model";
import type { ComponentProps, ReactNode } from "react";
import { cx } from "./cx";
import styles from "./badge.module.css";
// Neutral tag for project, agent, and event kinds.
export function Tag({
  children,
  mono,
  className,
  ...props
}: ComponentProps<"span"> & { mono?: boolean }) {
  return (
    <span
      {...props}
      className={cx(styles.badge, styles.tag, mono && styles.mono, className)}
    >
      {children}
    </span>
  );
}
// Task-state symbols. Decorative: the
// adjacent label carries the meaning. Running is static, never rotating.
const icons: Record<string, ReactNode> = {
  pending: <circle cx="8" cy="8" r="5.5" />,
  in_progress: (
    <>
      <circle cx="8" cy="8" r="5.5" />
      <path d="M8 2.5a5.5 5.5 0 0 1 0 11z" fill="currentColor" />
    </>
  ),
  blocked: <path d="M6 4.5v7M10 4.5v7" />,
  waiting: (
    <>
      <circle cx="8" cy="8" r="5.5" />
      <path d="M8 5v3l2 1.5" />
    </>
  ),
  escalated: <path d="M8 2l6 6-6 6-6-6z M8 5.5v3.5 M8 10.75v.5" />,
  done: <path d="M3 8.5l3 3 7-7" />,
};
export function StatusIcon({ state }: { state: string }) {
  const icon = icons[state];
  if (!icon) return null;
  return (
    <svg aria-hidden="true" viewBox="0 0 16 16" className={styles.icon}>
      {icon}
    </svg>
  );
}
export function StatusPill({
  task,
}: {
  task: Pick<Task, "state" | "status" | "waiting_on" | "not_before">;
}) {
  const state = taskStatus(task);
  return (
    <span
      className={cx(styles.badge, styles.status, styles[state] || styles.tag)}
      data-state={state}
      title={taskStatusTitle(task)}
    >
      <StatusIcon state={state} />
      <span>{taskStatusLabel(task)}</span>
    </span>
  );
}
