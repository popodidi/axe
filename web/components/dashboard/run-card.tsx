import { useId, useState } from "react";
import type { Run, Task } from "../../lib/model";
import { formatDuration, runCommands } from "../../lib/model";
import {
  Button,
  Command,
  Commands,
  EmptyState,
  LiveDot,
  StatusPill,
  Tag,
} from "../ui";
import styles from "./run-card.module.css";
export function RunList({
  runs,
  titles,
  tasks,
  now,
  onOpen,
  onCopy,
  onWatch,
  taskContext = false,
}: {
  runs: Run[];
  titles: Map<number, string>;
  tasks?: Map<number, Task>;
  now: number;
  onOpen: (taskId: number) => void;
  onCopy: (command: string) => void;
  onWatch: (runId: number) => void;
  taskContext?: boolean;
}) {
  if (!runs.length) return <EmptyState>No runs</EmptyState>;
  return (
    <div className={styles.list}>
      {runs.map((r) => (
        <RunCard
          key={r.id}
          run={r}
          title={titles.get(r.task_id)}
          task={tasks?.get(r.task_id)}
          now={now}
          onOpen={onOpen}
          onCopy={onCopy}
          onWatch={onWatch}
          taskContext={taskContext}
        />
      ))}
    </div>
  );
}
function RunCard({
  run: r,
  title,
  task,
  now,
  onOpen,
  onCopy,
  onWatch,
  taskContext,
}: {
  run: Run;
  title?: string;
  task?: Task;
  now: number;
  onOpen: (taskId: number) => void;
  onCopy: (command: string) => void;
  onWatch: (runId: number) => void;
  taskContext: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const detailsId = useId();
  const end = r.ended_at ? Date.parse(r.ended_at) : now;
  const { watch, attach } = runCommands(r);
  const activity = r.current_activity || "No activity recorded";
  return (
    <article
      className={styles.run}
      data-live={!r.ended_at}
      aria-label={`Run #${r.id}`}
    >
      {!taskContext && (
        <div className={styles.identity}>
          <Button variant="link" onClick={() => onOpen(r.task_id)}>
            #{r.task_id}
          </Button>
          <span className={styles.title}>{title}</span>
          <Tag>{r.project}</Tag>
          {task && <StatusPill task={task} />}
        </div>
      )}
      <div className={styles.summary}>
        <Button
          variant="ghost"
          aria-expanded={expanded}
          aria-controls={detailsId}
          aria-label={`Run #${r.id} details`}
          onClick={() => setExpanded(!expanded)}
        >
          <span aria-hidden="true">{expanded ? "⌄" : "›"}</span> Run #{r.id}
        </Button>
        <span className={styles.state}>
          <LiveDot live={!r.ended_at} />
          {r.ended_at ? "Finished" : "Running"}
        </span>
        <span className={styles.elapsed} title={"Started " + r.started_at}>
          {formatDuration(end - Date.parse(r.started_at))}
        </span>
        <Button onClick={() => onWatch(r.id)}>
          {r.ended_at ? "View log" : "Watch"}
        </Button>
        <span className={styles.activityText}>{activity}</span>
        <span className={styles.seen}>
          {r.last_activity_at
            ? "seen " +
              formatDuration(now - Date.parse(r.last_activity_at)) +
              " ago"
            : "not seen"}
        </span>
      </div>
      <div id={detailsId} hidden={!expanded} className={styles.details}>
        <p className={styles.result}>{activity}</p>
        <div className={styles.metadata}>
          <Tag>{r.agent || "Unknown agent"}</Tag>
          <Tag>{r.mode || "Unknown mode"}</Tag>
          {r.ended_at && <Tag mono>exit {r.exit_code ?? "?"}</Tag>}
        </div>
        <Commands>
          <Command
            label="watch"
            command={watch}
            copyLabel="Copy watch"
            onCopy={onCopy}
          />
          {attach && (
            <Command
              label="attach"
              command={attach}
              copyLabel="Copy attach"
              onCopy={onCopy}
            />
          )}
        </Commands>
      </div>
    </article>
  );
}
