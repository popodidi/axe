"use client";
import {
  memo,
  useMemo,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import Markdown from "../../app/markdown";
import type { Run } from "../../lib/model";
import {
  logSize,
  MAX_LOG_BYTES,
  MAX_LOG_LINES,
  type LogLine,
  type LogPage,
} from "../../lib/run-log";
import { buildTranscript, type TranscriptEntry } from "../../lib/transcript";
import { Button, CodeBlock, Tag } from "../ui";
import styles from "./run-log.module.css";

// Bot runs (`kind="bot"`) use /api/bot-runs and carry `bot` instead of a task.
type LogRun = Run & { bot?: string };
type Reply = { run: LogRun; page?: LogPage; unavailable?: string; retryable?: boolean; log_complete?: boolean };
export function RunLog({
  id,
  base,
  kind = "task",
  onClose,
}: {
  id: string;
  base: string;
  kind?: "task" | "bot";
  onClose: () => void;
}) {
  const [raw, setRaw] = useState(false);
  const [run, setRun] = useState<LogRun>();
  const [lines, setLines] = useState<LogLine[]>([]);
  const [connection, setConnection] = useState("Connecting");
  const [unavailable, setUnavailable] = useState("");
  const [error, setError] = useState("");
  const [paused, setPaused] = useState(false);
  const [full, setFull] = useState(false);
  const [beginning, setBeginning] = useState(false);
  const [ready, setReady] = useState(false);
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  const pane = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  const anchor = useRef<{ height: number; top: number } | null>(null);
  const state = useRef<{
    page?: LogPage;
    lines: LogLine[];
    busy: boolean;
    finished: boolean;
    failures: number;
    stopped: boolean;
  }>({ lines: [], busy: false, finished: false, failures: 0, stopped: false });
  const older = useRef<() => void>(() => {});
  const closeButton = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    closeButton.current?.focus();
    return () => previous?.focus();
  }, []);
  useLayoutEffect(() => {
    const el = pane.current;
    if (!el) return;
    if (anchor.current) {
      el.scrollTop =
        anchor.current.top + el.scrollHeight - anchor.current.height;
      anchor.current = null;
    } else if (follow.current) el.scrollTop = el.scrollHeight;
  }, [lines]);
  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const controller = new AbortController();
    const s = state.current;
    async function load(direction: "next" | "older") {
      if (s.busy || disposed || s.stopped) return;
      if (direction === "next" && s.page && (paused || full || s.finished))
        return;
      s.busy = true;
      setLoading(true);
      try {
        const q = new URLSearchParams();
        if (s.page) {
          q.set(
            direction === "older" ? "before" : "after",
            String(direction === "older" ? s.page.start : s.page.end),
          );
          q.set("generation", s.page.generation);
        }
        const response = await fetch(
          `${base}/api/${kind === "bot" ? "bot-runs" : "runs"}/${encodeURIComponent(id)}/log?${q}`,
          {
            signal: AbortSignal.any([
              controller.signal,
              AbortSignal.timeout(10000),
            ]),
          },
        );
        if (!response.ok) {
          const body = await response.json().catch(() => ({}));
          const detail = typeof body?.error === "string" ? body.error : "";
          if (disposed) return;
          const transient =
            [408, 429].includes(response.status) || response.status >= 500;
          if (!transient) {
            s.stopped = true;
            setError(
              response.status === 404 && detail.startsWith("no route for ")
                ? "API does not support run logs. Ask ops to upgrade the taskq API, then Retry."
                : response.status === 404 && /^(bot )?run \d+ not found$/.test(detail)
                  ? "Run not found. Check the run ID and API address, then Retry."
                  : `Log request failed (HTTP ${response.status}). Check API compatibility and access, then Retry.`,
            );
            setConnection("Unavailable");
            return;
          }
          throw Error(`HTTP ${response.status}`);
        }
        const reply: Reply = await response.json();
        if (disposed) return;
        s.failures = 0;
        setError("");
        setRun(reply.run);
        setConnection("Connected");
        if (!reply.page) {
          setUnavailable(reply.unavailable || "Log unavailable");
          s.stopped = !reply.retryable;
          if (reply.retryable) setConnection("Dispatcher unavailable");
          return;
        }
        const p = reply.page;
        setReady(true);
        const combined = !p.lines.length
          ? s.lines
          : direction === "older"
            ? [...p.lines, ...s.lines]
            : [...s.lines, ...p.lines];
        if (
          combined.length > MAX_LOG_LINES ||
          logSize(combined) > MAX_LOG_BYTES
        ) {
          if (direction === "next") {
            setFull(true);
            return;
          }
          // Explicitly move to the older window; no hidden eviction while following.
          s.lines = p.lines;
          s.page = p;
          follow.current = false;
          setFull(true);
          if (pane.current) pane.current.scrollTop = 0;
        } else {
          if (direction === "older" && pane.current) {
            anchor.current = {
              height: pane.current.scrollHeight,
              top: pane.current.scrollTop,
            };
            follow.current = false;
          }
          s.lines = combined;
          s.page = {
            ...p,
            start: direction === "older" ? p.start : (s.page?.start ?? p.start),
            end: direction === "older" ? (s.page?.end ?? p.end) : p.end,
          };
        }
        s.finished = (reply.log_complete ?? !!reply.run.ended_at) && s.page.end >= p.size;
        setBeginning(s.page.start === 0);
        setLines(s.lines);
        setUnavailable("");
      } catch (e) {
        if (!disposed) {
          s.failures++;
          s.stopped = s.failures > 3;
          setConnection(s.stopped ? "Disconnected" : "Reconnecting");
          setError(
            s.stopped
              ? `Connection failed after 3 retries (${String(e)}). Check the API connection, then Retry.`
              : `Connection interrupted (${String(e)}). Retrying ${s.failures}/3…`,
          );
        }
      } finally {
        s.busy = false;
        if (!disposed) setLoading(false);
      }
    }
    older.current = () => {
      void load("older");
    };
    async function tick() {
      await load("next");
      if (!disposed && !s.finished && !s.stopped && !paused && !full)
        timer = setTimeout(
          tick,
          s.failures ? 1000 * 2 ** (s.failures - 1) : 1000,
        );
    }
    void tick();
    return () => {
      disposed = true;
      controller.abort();
      clearTimeout(timer);
      older.current = () => {};
    };
  }, [base, id, kind, paused, full, revision]);
  const transcript = useMemo(() => buildTranscript(lines), [lines]);
  const status = !run
    ? error
      ? "Run unavailable"
      : "Loading run"
    : !run.ended_at
      ? "Running"
      : run.exit_code === 0
        ? "Completed"
        : run.exit_code === null
          ? "Ended · exit unknown"
          : `Failed · exit ${run.exit_code}`;
  return (
    <section
      className={styles.viewer}
      role="dialog"
      aria-modal="true"
      aria-label="Run log"
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          onClose();
        }
        if (e.key === "Tab") {
          const controls = Array.from(
            e.currentTarget.querySelectorAll<HTMLElement>(
              'button:not(:disabled), summary, a[href], [tabindex="0"]',
            ),
          );
          const visible = controls.filter((el) => el.getClientRects().length);
          const first = visible[0],
            last = visible.at(-1);
          if (e.shiftKey && document.activeElement === first) {
            e.preventDefault();
            last?.focus();
          }
          if (!e.shiftKey && document.activeElement === last) {
            e.preventDefault();
            first?.focus();
          }
        }
      }}
    >
      <header className={styles.header}>
        <Button size="sm" ref={closeButton} onClick={onClose}>
          Back to dashboard
        </Button>
        <h2>
          Run #{id}
          {run &&
            (kind === "bot"
              ? ` · Bot ${run.bot} · ${run.agent}`
              : ` · Task #${run.task_id} · ${run.agent}`)}
        </h2>
        {run && (
          <span>
            {run.started_at}
          </span>
        )}
        <Tag
          data-run-state={
            !run
              ? "unknown"
              : !run.ended_at
                ? "running"
                : run.exit_code === 0
                  ? "completed"
                  : run.exit_code === null
                    ? "unknown"
                    : "failed"
          }
        >
          {status}
        </Tag>
        <span role="status">
          {error
            ? connection
            : unavailable
              ? "Log unavailable"
              : paused
                ? "Paused"
                : full
                  ? "Window full · follow paused"
                  : run?.ended_at
                    ? "History"
                    : connection}
        </span>
        {(error || unavailable) && (
          <Button
            size="sm"
            onClick={() => {
              state.current.stopped = false;
              state.current.failures = 0;
              state.current.finished = false;
              setError("");
              setUnavailable("");
              setConnection("Connecting");
              setPaused(false);
              setFull(false);
              setRevision((n) => n + 1);
            }}
          >
            Retry
          </Button>
        )}
        {run && !run.ended_at && !error && !unavailable && (
          <Button size="sm" onClick={() => setPaused(!paused)}>
            {paused ? "Resume" : "Pause"}
          </Button>
        )}
        <Button
          size="sm"
          onClick={() => {
            follow.current = true;
            if (full) {
              state.current = {
                lines: [],
                busy: false,
                finished: false,
                failures: 0,
                stopped: false,
              };
              setLines([]);
              setFull(false);
              setRevision((n) => n + 1);
            }
            setPaused(false);
            if (pane.current)
              pane.current.scrollTop = pane.current.scrollHeight;
          }}
        >
          Jump to latest
        </Button>
        <Button size="sm" aria-pressed={raw} onClick={() => setRaw(!raw)}>
          {raw ? "Show conversation" : "Inspect raw events"}
        </Button>
      </header>
      <div
        className={styles.scroll}
        ref={pane}
        tabIndex={0}
        aria-label="Log output"
        onScroll={() => {
          const el = pane.current;
          if (el)
            follow.current =
              el.scrollHeight - el.scrollTop - el.clientHeight < 40;
        }}
      >
        {error && <p role="alert">{error}</p>}
        {unavailable ? (
          <p role="alert">{unavailable}. History unavailable.</p>
        ) : (
          <>
            <Button
              size="sm"
              disabled={beginning || loading || !ready || !!error}
              onClick={() => older.current()}
            >
              Load older history
            </Button>
            {beginning && <p>Beginning of log</p>}
            {full && (
              <p>
                Bounded window reached. Load older history to move back; Jump to
                latest to follow again.
              </p>
            )}
            {!lines.length && !error && (
              <p>
                {loading
                  ? "Loading log…"
                  : run?.ended_at
                    ? "No output recorded"
                    : "Waiting for output"}
              </p>
            )}
            {raw
              ? lines.map((line) => <RawRecord key={line.start} line={line} />)
              : transcript.map((entry) => (
                  <ConversationEntry key={entry.key} entry={entry} />
                ))}
          </>
        )}
      </div>
    </section>
  );
}

// Raw payloads are mounted only on request; signatures never enter the conversation.
const RawRecord = memo(function RawRecord({ line }: { line: LogLine }) {
  const [open, setOpen] = useState(false);
  return (
    <details
      className={styles.record}
      onToggle={(e) => setOpen(e.currentTarget.open)}
    >
      <summary>Raw event · offset {line.start}</summary>
      {open && <CodeBlock>{line.text}</CodeBlock>}
    </details>
  );
});
const ConversationEntry = memo(
  function ConversationEntry({ entry }: { entry: TranscriptEntry }) {
    return (
      <article
        className={styles.record}
        data-offset={entry.offset}
        data-kind={entry.kind}
        data-error={entry.error}
      >
        {entry.kind === "message" ? (
          <>
            <h3>{entry.label}</h3>
            <Markdown compact body={entry.body} />
          </>
        ) : entry.kind === "tool" ? (
          <details>
            <summary>
              <span className={styles.toolLabel}>{entry.label}</span>
              <span className={styles.toolStatus}>{entry.status}</span>
            </summary>
            {entry.input && (
              <>
                <h4>Input</h4>
                <CodeBlock>{entry.input}</CodeBlock>
              </>
            )}
            {entry.body && (
              <>
                <h4>Output</h4>
                <CodeBlock>{entry.body}</CodeBlock>
              </>
            )}
            {!entry.body && <p>No output recorded in this window.</p>}
          </details>
        ) : entry.kind === "thinking" ? (
          <details>
            <summary>Thinking · available text</summary>
            <Markdown compact body={entry.body} />
          </details>
        ) : (
          <>
            <h3>{entry.label}</h3>
            {entry.error ? (
              <CodeBlock>{entry.body}</CodeBlock>
            ) : (
              <details>
                <summary>Inspect event</summary>
                <CodeBlock>{entry.body}</CodeBlock>
              </details>
            )}
          </>
        )}
      </article>
    );
  },
  (before, after) => {
    const a = before.entry,
      b = after.entry;
    return (
      a.key === b.key &&
      a.offset === b.offset &&
      a.kind === b.kind &&
      a.label === b.label &&
      a.body === b.body &&
      a.input === b.input &&
      a.status === b.status &&
      a.error === b.error
    );
  },
);
