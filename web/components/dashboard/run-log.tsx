"use client";
import {
  memo,
  useCallback,
  useMemo,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type MouseEvent,
  type ReactNode,
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
import {
  clock,
  count,
  duration,
  epochMs,
  fullTime,
  runtimeOf,
  size,
  usageTotals,
  type RunFile,
  type RunMeta,
} from "../../lib/run-meta";
import { buildTranscript, type TranscriptEntry } from "../../lib/transcript";
import { usePromptView, type PromptKind } from "../../lib/prompt-view";
import { tocEntries } from "../../lib/doc-toc";
import {
  Button,
  CodeBlock,
  Tag,
  TocFold,
  TocRail,
  useHeadings,
  useScrollSpy,
} from "../ui";
import { PaneResizer } from "./detail-resizer";
import styles from "./run-log.module.css";

// Bot runs (`kind="bot"`) use /api/bot-runs and carry `bot` instead of a task.
type LogRun = Run & {
  bot?: string;
  dispatcher?: string;
  stop_reason?: string | null;
  claim?: string;
};
type Reply = {
  run: LogRun;
  page?: LogPage;
  unavailable?: string;
  retryable?: boolean;
  log_complete?: boolean;
};
export function RunLog({
  id,
  base,
  kind = "task",
  onClose,
  onOpen,
}: {
  id: string;
  base: string;
  kind?: "task" | "bot";
  onClose: () => void;
  /** Opens another dashboard view (URL parameters) from a panel link. */
  onOpen?: (params: Record<string, string | null>) => void;
}) {
  // Undefined until mounted: the stylesheet decides where the details rail
  // starts open (wide windows); after that the toggle owns it.
  const [details, setDetails] = useState<boolean>();
  const detailsRail = useRef<HTMLElement>(null);
  useLayoutEffect(() => {
    const shown = !!detailsRail.current?.getClientRects().length;
    setDetails(shown);
  }, []);
  const [meta, setMeta] = useState<RunMeta>();
  const [metaError, setMetaError] = useState("");
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
                ? "API does not support run logs. Upgrade the axe server, then Retry."
                : response.status === 404 &&
                    /^(bot )?run \d+ not found$/.test(detail)
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
          if (reply.retryable) setConnection("Node unavailable");
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
        s.finished =
          (reply.log_complete ?? !!reply.run.ended_at) && s.page.end >= p.size;
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
  // The details panel: fetched once, and again when the run ends (its exit,
  // usage and outcome are recorded then). Never blocks the log.
  const ended = !!run?.ended_at;
  useEffect(() => {
    const controller = new AbortController();
    fetch(
      `${base}/api/${kind === "bot" ? "bot-runs" : "runs"}/${encodeURIComponent(id)}/meta`,
      {
        signal: AbortSignal.any([
          controller.signal,
          AbortSignal.timeout(10000),
        ]),
      },
    )
      .then(async (r) => {
        if (!r.ok)
          throw Error(
            r.status === 404
              ? "this axe server predates run details"
              : `HTTP ${r.status}`,
          );
        const m: RunMeta = await r.json();
        if (!m || typeof m !== "object" || !m.run)
          throw Error("unexpected reply");
        setMeta(m);
        setMetaError("");
      })
      .catch((e: unknown) => {
        if (!controller.signal.aborted)
          setMetaError(e instanceof Error ? e.message : String(e));
      });
    return () => controller.abort();
  }, [base, id, kind, ended, revision]);
  const transcript = useMemo(() => buildTranscript(lines), [lines]);
  const startMs = epochMs(run?.started_at);
  const timed = transcript.some((e) => e.at !== undefined);
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
        <span role="status" className={styles.connection}>
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
        <span className={styles.actions}>
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
          <Button
            size="sm"
            aria-expanded={!!details}
            aria-controls="run-details"
            onClick={() => setDetails(!details)}
          >
            {details ? "Hide details" : "Details"}
          </Button>
        </span>
      </header>
      <div
        className={styles.body}
        data-details={details === undefined ? "auto" : String(details)}
      >
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
              {beginning && <p className={styles.note}>Beginning of log</p>}
              {full && (
                <p className={styles.note}>
                  Bounded window reached. Load older history to move back; Jump
                  to latest to follow again.
                </p>
              )}
              {!raw && transcript.length > 0 && !timed && (
                <p className={styles.note}>
                  Event times were not recorded for this run; runs started after
                  per-event times were added show them.
                </p>
              )}
              {!lines.length && !error && (
                <p className={styles.note}>
                  {loading
                    ? "Loading log…"
                    : run?.ended_at
                      ? "No output recorded"
                      : "Waiting for output"}
                </p>
              )}
              {raw
                ? lines.map((line) => (
                    <RawRecord key={line.start} line={line} day={startMs} />
                  ))
                : transcript.map((entry) => (
                    <ConversationEntry
                      key={entry.key}
                      entry={entry}
                      day={startMs}
                    />
                  ))}
            </>
          )}
        </div>
        <PaneResizer
          controls="run-details"
          label="Resize run details"
          pane="run"
        />
        <aside
          ref={detailsRail}
          id="run-details"
          className={styles.details}
          aria-label="Run details"
        >
          <RunDetails
            id={id}
            kind={kind}
            run={run}
            meta={meta}
            error={metaError}
            timed={timed}
            onOpen={onOpen}
          />
        </aside>
      </div>
    </section>
  );
}

/** An entry's time: short local time, the full timestamp on hover. */
function Stamp({ at, day }: { at?: number; day?: number }) {
  if (at === undefined) return <span className={styles.time} />;
  const since =
    day !== undefined && at >= day
      ? ` · ${duration(at - day)} into the run`
      : "";
  return (
    <time
      className={styles.time}
      dateTime={new Date(at).toISOString()}
      title={fullTime(at) + since}
    >
      {clock(at, day)}
    </time>
  );
}

// Raw payloads are mounted only on request; signatures never enter the conversation.
const RawRecord = memo(function RawRecord({
  line,
  day,
}: {
  line: LogLine;
  day?: number;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className={styles.record} data-kind="raw">
      <Stamp at={line.at} day={day} />
      <details
        className={styles.content}
        onToggle={(e) => setOpen(e.currentTarget.open)}
      >
        <summary>Raw event · offset {line.start}</summary>
        {open && <CodeBlock>{line.text}</CodeBlock>}
      </details>
    </div>
  );
});

/** A tool label is `<name> · <short args>`. */
function splitLabel(label: string) {
  const n = label.indexOf(" · ");
  return n < 0 ? [label, ""] : [label.slice(0, n), label.slice(n + 3)];
}

const ConversationEntry = memo(
  function ConversationEntry({
    entry,
    day,
  }: {
    entry: TranscriptEntry;
    day?: number;
  }) {
    const [name, args] = splitLabel(entry.label);
    const took =
      entry.at !== undefined &&
      entry.endAt !== undefined &&
      entry.endAt >= entry.at
        ? duration(entry.endAt - entry.at)
        : "";
    return (
      <article
        className={styles.record}
        data-offset={entry.offset}
        data-kind={entry.kind}
        data-role={
          entry.kind === "message" ? entry.label.toLowerCase() : undefined
        }
        data-error={entry.error}
      >
        <Stamp at={entry.at} day={day} />
        <div className={styles.content}>
          {entry.kind === "message" ? (
            <>
              <h3 className={styles.role}>{entry.label}</h3>
              <Markdown compact body={entry.body} />
            </>
          ) : entry.kind === "tool" ? (
            <details>
              <summary>
                <span className={styles.toolName}>{name}</span>
                {args && <span className={styles.toolArgs}>{args}</span>}
                <span className={styles.toolStatus}>
                  {entry.status}
                  {took && ` · ${took}`}
                </span>
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
          ) : entry.kind === "system" ? (
            <p className={styles.system}>
              <span className={styles.systemLabel}>{entry.label}</span>
              {entry.body && (
                <span className={styles.systemBody}>{entry.body}</span>
              )}
            </p>
          ) : (
            <>
              <h3 className={styles.role}>{entry.label}</h3>
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
        </div>
      </article>
    );
  },
  (before, after) => {
    const a = before.entry,
      b = after.entry;
    return (
      before.day === after.day &&
      a.key === b.key &&
      a.offset === b.offset &&
      a.kind === b.kind &&
      a.label === b.label &&
      a.body === b.body &&
      a.input === b.input &&
      a.status === b.status &&
      a.error === b.error &&
      a.at === b.at &&
      a.endAt === b.endAt
    );
  },
);

type Fact = [string, ReactNode, string?];
/** A rail group: a small label over aligned key/value rows; empty rows drop. */
function Facts({ title, facts }: { title: string; facts: Fact[] }) {
  const shown = facts.filter(
    ([, v]) => v !== "" && v !== null && v !== undefined,
  );
  if (!shown.length) return null;
  return (
    <section className={styles.group}>
      <h3 className={styles.groupLabel}>{title}</h3>
      <dl className={styles.facts}>
        {shown.map(([k, v, tip]) => (
          <div key={k} className={styles.fact}>
            <dt>{k}</dt>
            <dd title={tip ?? (typeof v === "string" ? v : undefined)}>{v}</dd>
          </div>
        ))}
      </dl>
    </section>
  );
}

const missing = (why: string) => <span className={styles.missing}>{why}</span>;
const mono = (text: string) => <span className={styles.mono}>{text}</span>;

/** A time with its full form on hover. */
function When({ at }: { at?: string | null }) {
  const ms = epochMs(at);
  if (ms === undefined) return null;
  return (
    <time dateTime={at!} title={fullTime(ms)}>
      {fullTime(ms)}
    </time>
  );
}

/** Same dashboard, other parameters: run links keep `?server=`. */
function hrefWith(params: Record<string, string | null>) {
  if (typeof window === "undefined") return "/";
  const q = new URLSearchParams(window.location.search);
  for (const [k, v] of Object.entries(params))
    if (v === null) q.delete(k);
    else q.set(k, v);
  return "?" + q.toString();
}

function Link({
  params,
  onOpen,
  children,
}: {
  params: Record<string, string | null>;
  onOpen?: (params: Record<string, string | null>) => void;
  children: ReactNode;
}) {
  return (
    <a
      href={hrefWith(params)}
      onClick={(e: MouseEvent) => {
        if (!onOpen || e.metaKey || e.ctrlKey || e.shiftKey || e.button) return;
        e.preventDefault();
        onOpen(params);
      }}
    >
      {children}
    </a>
  );
}

/** A prompt, folded; open, it shows in full (up to the recorded bound),
 * rendered as Markdown or as its raw text. */
function Prompt({
  title,
  kind,
  file,
}: {
  title: string;
  kind: PromptKind;
  file?: RunFile;
}) {
  const [view, setView] = usePromptView(kind);
  if (!file?.text && file?.text !== "")
    return (
      <div className={styles.prompt}>
        <span className={styles.promptTitle}>{title}</span>
        {missing(
          file?.unavailable ? capitalize(file.unavailable) : "Not available",
        )}
      </div>
    );
  return (
    <details className={styles.prompt}>
      <summary>
        <span className={styles.promptTitle}>{title}</span>
        <span className={styles.promptSize}>
          {size(file.bytes ?? file.text.length)}
          {file.source === "current" && " · current, not this run's"}
        </span>
      </summary>
      <div
        className={styles.promptView}
        role="group"
        aria-label={`${title} view`}
      >
        {(["rendered", "raw"] as const).map((v) => (
          <button
            key={v}
            type="button"
            aria-pressed={view === v}
            onClick={() => setView(v)}
          >
            {v === "raw" ? "Raw" : "Rendered"}
          </button>
        ))}
      </div>
      {file.source === "current" && (
        <p className={styles.missing}>
          This run predates per-run copies; this is the bot&apos;s latest system
          prompt.
        </p>
      )}
      {view === "raw" ? (
        <CodeBlock className={styles.promptText}>{file.text}</CodeBlock>
      ) : (
        <RenderedPrompt kind={kind} text={file.text} />
      )}
      {file.truncated && (
        <p className={styles.missing}>
          Showing the first {size(file.text.length)} of {size(file.bytes ?? 0)};
          the whole file is on the node.
        </p>
      )}
    </details>
  );
}

/** A prompt as Markdown in its scroll box, with a contents list for a long
 * one (components/ui/toc.tsx): beside the text when the box is wide, folded
 * above it when narrow. The section in view follows the box's scroll. */
function RenderedPrompt({ kind, text }: { kind: PromptKind; text: string }) {
  const box = useRef<HTMLDivElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const headings = useHeadings(body, text);
  const entries = useMemo(() => tocEntries(headings), [headings]);
  const active = useScrollSpy(entries, box);
  const jump = useCallback((id: string) => {
    const b = box.current;
    const h = document.getElementById(id);
    if (!b || !h) return;
    b.scrollTop +=
      h.getBoundingClientRect().top - b.getBoundingClientRect().top;
  }, []);
  const toc = { entries, active, onJump: jump };
  return (
    <div
      ref={box}
      className={styles.promptRendered}
      data-toc={entries.length ? "" : undefined}
    >
      <div className={styles.promptLayout}>
        <TocRail {...toc} className={styles.promptTocRail} />
        <TocFold {...toc} className={styles.promptTocFold} />
        <div ref={body} className={styles.promptBody}>
          <Markdown body={text} compact headingIds={`prompt-${kind}-`} />
        </div>
      </div>
    </div>
  );
}

const capitalize = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

function RunDetails({
  id,
  kind,
  run,
  meta,
  error,
  timed,
  onOpen,
}: {
  id: string;
  kind: "task" | "bot";
  run?: LogRun;
  meta?: RunMeta;
  error: string;
  timed: boolean;
  onOpen?: (params: Record<string, string | null>) => void;
}) {
  const start = meta?.start ?? {};
  const end = meta?.end ?? {};
  const rt = runtimeOf(meta, run?.agent);
  const startMs = epochMs(run?.started_at);
  const endMs = epochMs(run?.ended_at);
  // A live run's duration ticks; an ended one's is fixed.
  const [now, setNow] = useState<number>();
  useEffect(() => {
    if (endMs !== undefined) return;
    const tick = () => setNow(Date.now());
    const first = setTimeout(tick, 0);
    const timer = setInterval(tick, 1000);
    return () => {
      clearTimeout(first);
      clearInterval(timer);
    };
  }, [endMs]);
  const total = usageTotals(meta);
  const files = meta?.files;
  const filesMissing = files?.unavailable
    ? capitalize(files.unavailable)
    : files && !files.prompt
      ? "Node predates run details"
      : undefined;
  const repos = meta?.task?.repos ?? meta?.bot?.repos;
  const stopReason = run?.stop_reason;
  const apiError = (end as { api_error?: { class?: string; message?: string } })
    .api_error;
  const exit =
    run?.ended_at === null || !run
      ? ""
      : run.exit_code === null
        ? "unknown"
        : `code ${run.exit_code}`;
  const reason = [
    stopReason && stopReason !== "exit" ? stopReason : "",
    apiError ? apiError.class || apiError.message || "" : "",
    meta?.outcome?.note ?? "",
  ]
    .filter(Boolean)
    .join(" · ");
  const wakes = Object.entries(meta?.wakes ?? {})
    .map(([k, n]) => `${n} ${k}${n === 1 ? "" : "s"}`)
    .join(", ");
  return (
    <>
      {error && !meta && (
        <p className={styles.missing}>
          Run details unavailable ({error}). The log still works.
        </p>
      )}
      <Facts
        title="Run"
        facts={[
          ["Run", mono(`#${id}`)],
          kind === "task" && run
            ? [
                "Task",
                <Link
                  key="task"
                  params={{
                    run: null,
                    botrun: null,
                    view: "tasks",
                    task: String(run.task_id),
                    tab: "runs",
                  }}
                  onOpen={onOpen}
                >
                  #{run.task_id}
                  {meta?.task?.title ? ` ${meta.task.title}` : ""}
                </Link>,
                meta?.task?.title,
              ]
            : ["Task", ""],
          kind === "bot" && run?.bot
            ? [
                "Bot",
                <Link
                  key="bot"
                  params={{
                    run: null,
                    botrun: null,
                    view: "bots",
                    bot: run.bot,
                  }}
                  onOpen={onOpen}
                >
                  {run.bot}
                </Link>,
                run.bot,
              ]
            : ["Bot", ""],
          [
            "Project",
            meta?.task?.project ?? meta?.bot?.project ?? run?.project ?? "",
          ],
          ["Mode", run?.mode ?? ""],
          [
            "Started",
            run?.started_at ? <When key="s" at={run.started_at} /> : "",
            run?.started_at,
          ],
          [
            "Ended",
            run?.ended_at ? (
              <When key="e" at={run.ended_at} />
            ) : run ? (
              missing("Still running")
            ) : (
              ""
            ),
            run?.ended_at ?? undefined,
          ],
          [
            "Duration",
            startMs !== undefined && (endMs ?? now) !== undefined
              ? duration((endMs ?? now)! - startMs) + (endMs ? "" : " so far")
              : "",
          ],
          ["Exit", exit],
          ["Reason", reason],
          [
            "Task state",
            meta?.task_state ? meta.task_state.replace("_", " ") : "",
          ],
          ["Wakes", wakes],
        ]}
      />
      <Facts
        title="Runtime"
        facts={[
          ["Runtime", rt.runtime],
          ["Harness", rt.harness],
          ["Version", rt.version],
          ["Model", rt.model || (meta ? missing("Not recorded") : "")],
          ["Effort", rt.effort || (meta ? missing("Harness default") : "")],
          ["Rule", rt.rule],
          ["Permission", rt.permission],
          [
            "Session",
            run?.session_id ? mono(run.session_id) : "",
            run?.session_id ?? undefined,
          ],
          [
            "Node",
            run?.dispatcher ? mono(run.dispatcher) : "",
            run?.dispatcher,
          ],
          [
            "Claim",
            run?.claim ? mono(String(run.claim)) : "",
            run?.claim ? String(run.claim) : undefined,
          ],
          [
            "Event times",
            timed
              ? files?.times
                ? "Recorded as the node saw each line"
                : "From the harness's records"
              : run
                ? missing("Not recorded for this run")
                : "",
          ],
        ]}
      />
      <Facts
        title="Usage"
        facts={
          total
            ? [
                ["Cost", `$${total.cost.toFixed(2)}`],
                ["Input", `${count(total.input)} tokens`],
                ["Output", `${count(total.output)} tokens`],
                ["Cache read", `${count(total.cache_read)} tokens`],
                ["Cache write", `${count(total.cache_write)} tokens`],
                [
                  "Turns",
                  meta?.usage?.turns != null ? String(meta.usage.turns) : "",
                ],
                ["Models", meta!.usage!.models.map((m) => m.model).join(", ")],
              ]
            : [
                [
                  "Usage",
                  meta
                    ? missing(
                        run?.ended_at
                          ? meta.usage?.reason
                            ? `Not recorded (${meta.usage.reason.replace(/_/g, " ")})`
                            : "Not recorded"
                          : "Recorded when the run ends",
                      )
                    : "",
                ],
              ]
        }
      />
      <Facts
        title="Context"
        facts={[
          [
            "Labels",
            meta?.task?.labels?.length ? meta.task.labels.join(", ") : "",
          ],
          [
            "Repos",
            repos?.length ? repos.join(", ") : meta ? missing("None") : "",
          ],
          ["Owner", meta?.task?.owner ?? ""],
          [
            "Run folder",
            typeof start.worktree === "string" ? mono(start.worktree) : "",
            start.worktree as string | undefined,
          ],
          ["Log", run?.log_path ? mono(run.log_path) : "", run?.log_path],
        ]}
      />
      {meta && (
        <section className={styles.group}>
          <h3 className={styles.groupLabel}>Prompts</h3>
          {filesMissing ? (
            <p className={styles.missing}>{filesMissing}</p>
          ) : (
            <>
              <Prompt
                title={
                  kind === "bot"
                    ? "System prompt"
                    : "System prompt · run instructions"
                }
                kind="system"
                file={files?.system_prompt}
              />
              <Prompt title="User prompt" kind="user" file={files?.prompt} />
            </>
          )}
        </section>
      )}
    </>
  );
}
