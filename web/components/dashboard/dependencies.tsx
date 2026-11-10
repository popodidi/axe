import type { DepRef, Task } from "../../lib/model";
import { waitReason, taskStatusTitle } from "../../lib/model";
import { Button, SectionLabel, StatusPill, Tag } from "../ui";
import styles from "./dependencies.module.css";
// Task detail: prerequisites (blocked_by) and dependents (blocks) as links with
// their status, plus why the task cannot be claimed yet.
function DepList({
  label,
  refs,
  tasks,
  onOpen,
}: {
  label: string;
  refs: DepRef[];
  tasks: Task[];
  onOpen: (id: number) => void;
}) {
  return (
    <div className={styles.list} aria-label={label} role="list">
      <SectionLabel>{label}</SectionLabel>
      {refs.length ? (
        refs.map((d) => (
          <div className={styles.row} key={d.id} role="listitem">
            <Button
              variant="link"
              className={styles.id}
              onClick={() => onOpen(d.id)}
            >
              #{d.id}
            </Button>
            <StatusPill task={tasks.find((t) => t.id === d.id) || d} />
            <span className={styles.title} title={d.title}>
              {d.title}
            </span>
            <Tag>{d.project}</Tag>
          </div>
        ))
      ) : (
        <span className={styles.none}>none</span>
      )}
    </div>
  );
}
export function Dependencies({
  task,
  tasks,
  onOpen,
}: {
  task: Task;
  tasks: Task[];
  onOpen: (id: number) => void;
}) {
  const reason = waitReason(task);
  const deps = task.deps || { blocked_by: [], blocks: [] };
  if (!reason && !deps.blocked_by.length && !deps.blocks.length) return null;
  return (
    <section className={styles.deps} aria-label="Dependencies">
      {reason && <p className={styles.reason} title={taskStatusTitle(task)}>{reason}</p>}
      {deps.blocked_by.length || deps.blocks.length ? (
        <>
          <DepList label="Blocked by" tasks={tasks} refs={deps.blocked_by} onOpen={onOpen} />
          <DepList label="Blocks" tasks={tasks} refs={deps.blocks} onOpen={onOpen} />
        </>
      ) : null}
    </section>
  );
}
