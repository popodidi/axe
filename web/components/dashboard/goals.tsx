"use client";
import { useLive } from "../../lib/live-load";
import Markdown from "../../app/markdown";
import {
  Alert,
  Button,
  Command,
  Commands,
  EmptyState,
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
  isIdle,
  linkedTasks,
  openTasks,
  selectGoals,
  usd,
  type Goal,
  type GoalDetail,
  type GoalEvent,
  type Meter,
} from "../../lib/goals";
import { useGoalTaskFilter, type GoalTaskFilter } from "../../lib/goal-task-filter";
import { Facts, ItemPane, ListEmpty, PaneSection, StateBadge, type Tone } from "./side-pane";
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
export function useGoals(base: string, enabled: boolean, revision: number): GoalListData {
  const live = useLive<Goal[]>(
    (signal) => get<Goal[]>(base + "/api/goals", signal),
    base,
    revision,
    enabled,
  );
  return { goals: live.value, error: live.error };
}
const tones: Record<string, Tone> = { proposed: "pending", active: "active", paused: "warn", done: "ok" };
function State({ state }: { state: string }) {
  return <StateBadge tone={tones[state] ?? "neutral"}>{title(state)}</StateBadge>;
}
/** The goal's state, and Idle when it has had no next action for 24h. */
function GoalState({ goal }: { goal: Goal }) {
  return (
    <span className={styles.states}>
      <State state={goal.state} />
      {isIdle(goal) && (
        <span title={`No open task, goal wake or goal schedule since ${when(goal.idle_since ?? null)}`}>
          <StateBadge tone="warn">Idle</StateBadge>
        </span>
      )}
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
  onClearFilters,
}: {
  state: GoalListData;
  params: URLSearchParams;
  onOpen: (name: string, keyboard: boolean) => void;
  onClearFilters?: () => void;
}) {
  const selected = params.get("goal");
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
                  aria-selected={g.name === selected}
                  onClick={(e) => onOpen(g.name, e.detail === 0)}
                >
                  <Td>
                    {/* Keyboard target; clicks bubble to the row. */}
                    <button type="button" className={styles.rowButton}>
                      {g.name}
                    </button>
                    <div className={styles.sub}>{g.project}</div>
                  </Td>
                  <Td>
                    <GoalState goal={g} />
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
            <ListEmpty
              total={goals.length}
              noun="goals"
              empty="No goals yet: set one with axe goal add"
              onClear={onClearFilters}
            />
          )}
        </Panel>
      )}
    </div>
  );
}
type Detail = { goal: GoalDetail; trajectory: GoalEvent[] };
// One goal in the side pane; a linked task opens in a pane stacked right.
export function GoalPane({
  base,
  name,
  revision,
  selectedTask,
  onClose,
  onOpenTask,
  onCopy,
}: {
  base: string;
  name: string;
  revision: number;
  /** The task open in the right-hand detail pane, if any. */
  selectedTask: string | null;
  onClose: () => void;
  onOpenTask: (id: number) => void;
  onCopy: (command: string) => void;
}) {
  const live = useLive<Detail>(
    async (signal) => {
      const path = `${base}/api/goals/${encodeURIComponent(name)}`;
      const [goal, trajectory] = await Promise.all([
        get<GoalDetail>(path, signal),
        get<GoalEvent[]>(`${path}/events?trajectory=1&limit=${TRAJECTORY_LIMIT}`, signal),
      ]);
      return { goal, trajectory };
    },
    `${base} ${name}`,
    revision,
  );
  const detail = live.value;
  const error = live.error;
  const d = detail?.goal.name === name ? detail : null;
  const g = d?.goal;
  const open = g ? openTasks(g.tasks) : [];
  const linked = g ? linkedTasks(g.tasks) : [];
  const [taskFilter, setTaskFilter] = useGoalTaskFilter();
  const shown = taskFilter === "all" ? linked : open;
  return (
    <ItemPane
      id="goal-detail"
      label="Goal detail"
      kind="Goal"
      badge={g && <GoalState goal={g} />}
      title={name}
      onClose={onClose}
    >
      {error && <Alert>Unable to load goal: {error}. Retrying on the next change.</Alert>}
      {!d || !g ? (
        !error && <Skeleton label="Loading goal…" lines={8} />
      ) : (
        <>
          <Facts
            items={[
              { label: "Project", value: g.project },
              { label: "Bots", value: g.bots.length ? g.bots.join(", ") : "None" },
              {
                label: `Budget (last ${g.budget?.window.days ?? 7} days)`,
                value: <Bar meter={costMeter(g)} label="Cost this week" />,
              },
              { label: "Attention", value: <Bar meter={attentionMeter(g)} label="Attention this week" /> },
              { label: "Deadline", value: deadline(g.deadline) },
              { label: "Next goal wake", value: g.next_wake ? when(g.next_wake) : "None scheduled" },
              { label: "Updated", value: when(g.updated_at) },
            ]}
          />
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
          <PaneSection title="Intent">
            {g.intent.trim() ? (
              <Markdown compact body={g.intent} />
            ) : (
              <p className={styles.note}>None</p>
            )}
          </PaneSection>
          <Panel aria-label="Linked tasks">
            <PanelHead
              label="Linked tasks"
              meta={
                <span className={styles.taskMeta}>
                  {`${open.length} open · ${g.tasks.length} linked`}
                  <span role="group" aria-label="Show tasks" className={styles.taskFilter}>
                    {(["open", "all"] as GoalTaskFilter[]).map((f) => (
                      <button
                        key={f}
                        type="button"
                        aria-pressed={taskFilter === f}
                        onClick={() => setTaskFilter(f)}
                      >
                        {title(f)}
                      </button>
                    ))}
                  </span>
                </span>
              }
            />
            {shown.length ? (
              <Table>
                <tbody>
                  {shown.map((t) => (
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
              <EmptyState>
                {taskFilter === "all" || !g.tasks.length
                  ? "No linked tasks"
                  : "No open linked tasks"}
              </EmptyState>
            )}
          </Panel>
          <PaneSection title="Trajectory">
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
          </PaneSection>
        </>
      )}
    </ItemPane>
  );
}
