"use client";
import { useEffect, useState } from "react";
import Markdown from "../../app/markdown";
import {
  Alert,
  Button,
  Command,
  Commands,
  EmptyState,
  Kbd,
  Panel,
  PanelHead,
  Skeleton,
  StatusPill,
  Table,
  Tag,
  Td,
  Th,
} from "../ui";
import { command } from "../../lib/model";
import {
  TRAJECTORY_LIMIT,
  attentionMeter,
  costMeter,
  deadline,
  linkedTasks,
  openTasks,
  selectGoals,
  usd,
  type Goal,
  type GoalDetail,
  type GoalEvent,
  type Meter,
} from "../../lib/goals";
import styles from "./goals.module.css";
// Goals: durable outcomes bots share (docs/design/goals.md). Read-only; the
// owner and bots change goals with `axe goal`.
const title = (s: string) => s[0].toUpperCase() + s.slice(1);
const when = (s: string | null) => (s ? new Date(s).toLocaleString() : "—");
async function get<T>(url: string, signal: AbortSignal): Promise<T> {
  const r = await fetch(url, { signal });
  if (!r.ok) throw Error(`HTTP ${r.status}`);
  return r.json();
}
export type GoalListData = { goals: Goal[] | null; error: string };
/** Every goal with its 7-day spend; reloads on every server change. */
export function useGoals(base: string, enabled: boolean, revision: number) {
  const [state, setState] = useState<GoalListData>({ goals: null, error: "" });
  useEffect(() => {
    if (!enabled) return;
    const c = new AbortController();
    get<Goal[]>(base + "/api/goals", c.signal)
      .then((goals) => {
        if (!c.signal.aborted) setState({ goals, error: "" });
      })
      .catch((e) => {
        if (!c.signal.aborted) setState((s) => ({ ...s, error: String(e) }));
      });
    return () => c.abort();
  }, [base, enabled, revision]);
  return state;
}
function State({ state }: { state: string }) {
  return (
    <span className={styles.state} data-state={state}>
      {title(state)}
    </span>
  );
}
function Bar({ meter, label }: { meter: Meter; label: string }) {
  return (
    <div className={styles.meter}>
      <span className={styles.mono}>{meter.text}</span>
      {meter.share !== null && (
        <span
          className={styles.track}
          role="meter"
          aria-label={label}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(meter.share * 100)}
          aria-valuetext={meter.text}
        >
          {/* SVG geometry, not an inline style: components style through CSS modules. */}
          <svg aria-hidden="true" viewBox="0 0 100 1" preserveAspectRatio="none" className={styles.bar}>
            <rect className={styles.fill} data-over={meter.over} width={meter.share * 100} height="1" />
          </svg>
        </span>
      )}
    </div>
  );
}
export function GoalList({
  state: { goals, error },
  params,
  onOpen,
}: {
  state: GoalListData;
  params: URLSearchParams;
  onOpen: (name: string) => void;
}) {
  const filtered = goals && selectGoals(goals, params);
  return (
    <div className={styles.stack}>
      {error && <Alert>Unable to load goals: {error}. Retrying automatically.</Alert>}
      {!goals ? (
        <Skeleton label="Loading goals…" lines={4} />
      ) : (
        <Panel aria-label="Goals">
          <PanelHead
            label="Goals"
            meta="Spend over the last 7 days · Read-only · manage with axe goal"
          />
          <Table>
            <thead>
              <tr>
                <Th>Goal</Th>
                <Th>State</Th>
                <Th className={styles.optional}>Bots</Th>
                <Th>Budget</Th>
                <Th className={styles.optional}>Attention</Th>
                <Th className={styles.optional}>Deadline</Th>
              </tr>
            </thead>
            <tbody>
              {filtered?.map((g) => (
                <tr
                  key={g.name}
                  id={"goal-" + g.name}
                  data-interactive=""
                  onClick={() => onOpen(g.name)}
                >
                  <Td>
                    {/* Keyboard target; clicks bubble to the row. */}
                    <button type="button" className={styles.rowButton}>
                      {g.name}
                    </button>
                    <div className={styles.sub}>{g.project}</div>
                  </Td>
                  <Td>
                    <State state={g.state} />
                  </Td>
                  <Td className={styles.optional}>
                    {g.bots.length ? g.bots.join(", ") : <span className={styles.sub}>None</span>}
                  </Td>
                  <Td>
                    <Bar meter={costMeter(g)} label={`${g.name} cost this week`} />
                  </Td>
                  <Td className={styles.optional}>
                    <Bar meter={attentionMeter(g)} label={`${g.name} attention this week`} />
                  </Td>
                  <Td mono className={styles.optional}>
                    {deadline(g.deadline)}
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
          {!filtered?.length && (
            <EmptyState>{goals.length ? "No matching goals" : "No goals yet."}</EmptyState>
          )}
        </Panel>
      )}
    </div>
  );
}
type Detail = { goal: GoalDetail; trajectory: GoalEvent[] };
export function GoalDetailPage({
  base,
  name,
  revision,
  selectedTask,
  onBack,
  onOpenTask,
  onCopy,
}: {
  base: string;
  name: string;
  revision: number;
  /** The task open in the right-hand detail pane, if any. */
  selectedTask: string | null;
  onBack: () => void;
  onOpenTask: (id: number) => void;
  onCopy: (command: string) => void;
}) {
  const [detail, setDetail] = useState<Detail | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    const c = new AbortController();
    const path = `${base}/api/goals/${encodeURIComponent(name)}`;
    Promise.all([
      get<GoalDetail>(path, c.signal),
      get<GoalEvent[]>(`${path}/events?trajectory=1&limit=${TRAJECTORY_LIMIT}`, c.signal),
    ])
      .then(([goal, trajectory]) => {
        if (c.signal.aborted) return;
        setDetail({ goal, trajectory });
        setError("");
      })
      .catch((e) => {
        if (!c.signal.aborted) setError(String(e));
      });
    return () => c.abort();
  }, [base, name, revision]);
  const d = detail?.goal.name === name ? detail : null;
  const g = d?.goal;
  const open = g ? openTasks(g.tasks) : [];
  const linked = g ? linkedTasks(g.tasks) : [];
  return (
    <section className={styles.stack} id="goal-detail" aria-label="Goal detail">
      <header className={styles.header}>
        <div className={styles.top}>
          <span className={styles.id}>Goal</span>
          {g && <State state={g.state} />}
          <Button variant="ghost" className={styles.back} onClick={onBack}>
            Back to goals <Kbd>Esc</Kbd>
          </Button>
        </div>
        <h2 className={styles.title} tabIndex={-1}>
          {name}
        </h2>
      </header>
      {error && <Alert>Unable to load goal: {error}. Retrying on the next change.</Alert>}
      {!d || !g ? (
        !error && <Skeleton label="Loading goal…" lines={8} />
      ) : (
        <>
          <dl className={styles.facts}>
            <div>
              <dt>Project</dt>
              <dd>{g.project}</dd>
            </div>
            <div>
              <dt>Bots</dt>
              <dd>{g.bots.length ? g.bots.join(", ") : "None"}</dd>
            </div>
            <div>
              <dt>Budget (last {g.budget?.window.days ?? 7} days)</dt>
              <dd>
                <Bar meter={costMeter(g)} label="Cost this week" />
              </dd>
            </div>
            <div>
              <dt>Attention</dt>
              <dd>
                <Bar meter={attentionMeter(g)} label="Attention this week" />
              </dd>
            </div>
            <div>
              <dt>Deadline</dt>
              <dd>{deadline(g.deadline)}</dd>
            </div>
            <div>
              <dt>Next goal wake</dt>
              <dd>{g.next_wake ? when(g.next_wake) : "None scheduled"}</dd>
            </div>
            <div>
              <dt>Updated</dt>
              <dd>{when(g.updated_at)}</dd>
            </div>
          </dl>
          {g.budget && (
            <p className={styles.note}>
              Spend by source: tasks {usd(g.budget.cost_tasks)}
              {Object.entries(g.budget.cost_bots).map(([b, c]) => ` · bot ${b} ${usd(c)}`)}
              {(g.budget.unattributed_runs > 0 || g.budget.unknown_runs > 0) &&
                ` · not counted: ${g.budget.unattributed_runs} mixed-wake bot runs, ${g.budget.unknown_runs} runs with unknown usage`}
            </p>
          )}
          <Commands>
            <Command
              label="show"
              command={command(g.project, ["axe", "goal", "show", name])}
              copyLabel="Copy goal show"
              onCopy={onCopy}
            />
            <Command
              label="log"
              command={command(g.project, ["axe", "goal", "log", name])}
              copyLabel="Copy goal log"
              onCopy={onCopy}
            />
          </Commands>
          <section className={styles.block} aria-label="Intent">
            <h3 className={styles.blockTitle}>Intent</h3>
            {g.intent.trim() ? (
              <Markdown compact body={g.intent} />
            ) : (
              <p className={styles.note}>None</p>
            )}
          </section>
          <Panel aria-label="Linked tasks">
            <PanelHead
              label="Linked tasks"
              meta={`${open.length} open · ${g.tasks.length} linked`}
            />
            {linked.length ? (
              <Table>
                <tbody>
                  {linked.map((t) => (
                    <tr
                      key={t.id}
                      id={"goal-task-" + t.id}
                      data-interactive=""
                      aria-selected={selectedTask === String(t.id)}
                      onClick={() => onOpenTask(t.id)}
                    >
                      <Td mono>
                        {/* Keyboard target; clicks bubble to the row. Closing the
                            task pane returns focus here. */}
                        <button
                          type="button"
                          className={styles.rowButton}
                          data-open-task={t.id}
                        >
                          #{t.id}
                        </button>
                      </Td>
                      <Td>
                        <StatusPill
                          task={{ state: t.state, status: t.state, waiting_on: [], not_before: null }}
                        />
                      </Td>
                      <Td>{t.title}</Td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            ) : (
              <EmptyState>No linked tasks</EmptyState>
            )}
          </Panel>
          <section className={styles.block} aria-label="Trajectory">
            <h3 className={styles.blockTitle}>Trajectory</h3>
            {d.trajectory.length ? (
              <ol className={styles.trajectory}>
                {d.trajectory.map((e) => (
                  <li key={e.id} id={"goal-event-" + e.id} className={styles.event} data-kind={e.kind}>
                    <div className={styles.eventHead}>
                      <span className={styles.kind} data-kind={e.kind}>
                        {title(e.kind)}
                      </span>
                      <time className={styles.mono} dateTime={e.at}>
                        {when(e.at)}
                      </time>
                      <Tag>{e.actor}</Tag>
                      {e.task_id !== null && (
                        <Button
                          variant="ghost"
                          size="sm"
                          aria-label={`Open task #${e.task_id}`}
                          data-open-task={e.task_id}
                          onClick={() => onOpenTask(e.task_id!)}
                        >
                          #{e.task_id}
                        </Button>
                      )}
                    </div>
                    <Markdown compact body={e.body} />
                  </li>
                ))}
              </ol>
            ) : (
              <EmptyState>No trajectory yet</EmptyState>
            )}
          </section>
        </>
      )}
    </section>
  );
}
