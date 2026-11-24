import type { Task } from "../../lib/model";
import { EmptyState, StatusPill, Table, Tag, Td, Th } from "../ui";
import styles from "./cluster-list.module.css";
/**
 * The tasks behind one Stats scatter dot or cluster, newest first. A row opens
 * its task in the same pane; closing the task comes back here.
 */
export function ClusterList({
  ids,
  tasks,
  onOpen,
}: {
  ids: number[];
  tasks: Task[];
  /** `keyboard`: opened with Enter or Space, so focus moves to the detail. */
  onOpen: (id: number, keyboard: boolean) => void;
}) {
  const byId = new Map(tasks.map((t) => [t.id, t]));
  const rows = [...new Set(ids)].sort((a, b) => b - a);
  if (!rows.length) return <EmptyState>No tasks in this selection.</EmptyState>;
  return (
    <Table>
      <thead>
        <tr>
          <Th scope="col">ID</Th>
          <Th scope="col">Task</Th>
          <Th scope="col">Status</Th>
        </tr>
      </thead>
      <tbody>
        {rows.map((id) => {
          const t = byId.get(id);
          return (
            <tr
              key={id}
              data-interactive=""
              onClick={(e) => onOpen(id, e.detail === 0)}
            >
              <Td mono>
                {/* Keyboard target; clicks bubble to the row. Closing the task
                    returns focus here. */}
                <button type="button" className={styles.open} data-open-task={id}>
                  #{id}
                </button>
              </Td>
              <Td truncate title={t?.title}>
                {t ? (
                  <>
                    <span className={styles.title}>{t.title}</span>{" "}
                    <Tag>{t.project}</Tag>
                  </>
                ) : (
                  <span className={styles.unknown}>Not in the current task list</span>
                )}
              </Td>
              <Td>{t && <StatusPill task={t} />}</Td>
            </tr>
          );
        })}
      </tbody>
    </Table>
  );
}
