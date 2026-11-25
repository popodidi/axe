import { formatLog, type LogLine } from "./run-log";
import { count, duration } from "./run-meta";
type Obj = Record<string, unknown>;
const obj = (v: unknown): Obj =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Obj) : {};
const str = (v: unknown) => (typeof v === "string" ? v : "");
const pretty = (v: unknown): string =>
  typeof v === "string" ? v : (JSON.stringify(v, null, 2) ?? "");
const contentText = (v: unknown): string =>
  Array.isArray(v)
    ? v.map((c) => str(obj(c).text) || pretty(c)).join("\n")
    : pretty(v);
export type TranscriptEntry = {
  key: string;
  offset: number;
  kind: "message" | "tool" | "thinking" | "event" | "system";
  label: string;
  body: string;
  input?: string;
  status?: string;
  error?: boolean;
  /** When the entry began (unix ms): the record's own timestamp, else when
   * the dispatcher saw its line; absent when neither was recorded. */
  at?: number;
  /** When a tool's result arrived. */
  endAt?: number;
};
type Fields = Omit<TranscriptEntry, "key" | "offset" | "at" | "endAt">;
/** One-line harness events worth a place in the timeline; others stay out. */
function systemEvent(
  v: Obj,
): Pick<TranscriptEntry, "label" | "body" | "error"> | undefined {
  const join = (...parts: string[]) => parts.filter(Boolean).join(" · ");
  switch (str(v.subtype)) {
    case "init":
      return {
        label: "Session started",
        body: join(
          str(v.model),
          v.claude_code_version
            ? `Claude Code ${str(v.claude_code_version)}`
            : "",
          str(v.permissionMode),
        ),
      };
    case "task_started":
      return { label: "Background task started", body: str(v.description) };
    case "task_notification":
      return {
        label: `Background task ${str(v.status) || "update"}`,
        body: str(v.summary),
      };
    case "code_change_published":
      return {
        label: `Change ${str(v.action) || "published"}`,
        body: join(str(v.url), str(v.branch)),
      };
    case "vcs_state_changed":
      return { label: `Git ${str(v.kind) || "change"}`, body: str(v.cwd) };
    case "permission_denied":
      return {
        label: `Permission denied${v.tool_name ? ` · ${str(v.tool_name)}` : ""}`,
        body: str(v.message) || str(v.decision_reason),
        error: true,
      };
    case "api_retry":
      return {
        label: `API retry ${String(v.attempt ?? "?")}/${String(v.max_retries ?? "?")}`,
        body: join(
          v.error_status ? `HTTP ${String(v.error_status)}` : "",
          str(v.error),
        ),
      };
    case "notification":
      return { label: "Notification", body: str(v.text) };
    case "compact_boundary":
      return {
        label: "Context compacted",
        body: str(obj(v.compact_metadata).trigger),
      };
  }
  return undefined;
}
// Replay the bounded window so prepends and retries produce the same transcript.
// Coalesce only explicit protocol IDs, never identical message text.
export function buildTranscript(lines: LogLine[]): TranscriptEntry[] {
  const entries = new Map<string, TranscriptEntry>();
  let streamId = "";
  const streams = new Map<string, Obj>();
  const finalized = new Set<string>();
  // The current record's time: its own `timestamp`, else when it was seen.
  let now: number | undefined;
  function put(key: string, line: LogLine, value: Fields, ended = false) {
    const old = entries.get(key);
    entries.set(key, {
      key,
      offset: old?.offset ?? line.start,
      at: old?.at ?? now,
      endAt: ended ? now : old?.endAt,
      ...value,
    });
  }
  function block(c: Obj, role: string, key: string, line: LogLine) {
    const type = str(c.type);
    if (type === "text") {
      put(key, line, {
        kind: "message",
        label: role === "user" ? "User" : "Assistant",
        body: str(c.text),
      });
    } else if (type === "thinking" || type === "redacted_thinking") {
      if (str(c.thinking).trim())
        put(key, line, {
          kind: "thinking",
          label: "Thinking",
          body: str(c.thinking),
        });
      // A signature-only final record must not erase text already streamed.
    } else if (type === "tool_use") {
      const toolKey = str(c.id) ? `tool:${c.id}` : key;
      const input = obj(c.input);
      const summary =
        str(input.description) ||
        str(input.command) ||
        str(input.file_path) ||
        str(input.path);
      const old = entries.get(toolKey);
      put(toolKey, line, {
        kind: "tool",
        label: `${str(c.name) || "Tool"}${summary ? ` · ${summary.slice(0, 160)}` : ""}`,
        input: pretty(c.input),
        body: old?.body ?? "",
        status: old?.status ?? "Result not recorded",
        error: old?.error,
      });
    } else if (type === "tool_result") {
      const toolKey = str(c.tool_use_id) ? `tool:${c.tool_use_id}` : key;
      const old = entries.get(toolKey);
      put(
        toolKey,
        line,
        {
          kind: "tool",
          label:
            old?.label ?? "Tool result · invocation outside loaded history",
          input: old?.input,
          body: contentText(c.content),
          status: c.is_error ? "Failed" : "Completed",
          error: !!c.is_error,
        },
        true,
      );
    } else
      put(key, line, {
        kind: "event",
        label: type || "Unknown content",
        body: pretty(c),
      });
  }
  for (const line of lines) {
    let v: Obj;
    try {
      v = obj(JSON.parse(line.text));
    } catch {
      v = {};
    }
    const type = str(v.type);
    const stamp =
      typeof v.timestamp === "string" ? Date.parse(v.timestamp) : NaN;
    now = Number.isFinite(stamp) ? stamp : line.at;
    const fallback = () =>
      formatLog(line).forEach((b, n) =>
        put(`raw:${line.start}:${n}`, line, {
          kind: "event",
          ...b,
          error: b.error || !!v.error || !!v.is_error,
        }),
      );
    if (line.fragment || !type) {
      fallback();
      continue;
    }
    if (type === "stream_event") {
      const event = obj(v.event),
        eventType = str(event.type);
      if (eventType === "message_start")
        streamId = str(obj(event.message).id) || `offset:${line.start}`;
      else if (eventType === "content_block_start") {
        const key = `claude:${streamId}:${event.index}`,
          c = { ...obj(event.content_block) };
        streams.set(key, c);
        block(c, "assistant", key, line);
      } else if (eventType === "content_block_delta") {
        const key = `claude:${streamId}:${event.index}`,
          delta = obj(event.delta),
          c = streams.get(key);
        if (!c) {
          fallback();
          continue;
        }
        if (delta.type === "text_delta") c.text = str(c.text) + str(delta.text);
        else if (delta.type === "thinking_delta")
          c.thinking = str(c.thinking) + str(delta.thinking);
        else if (delta.type === "input_json_delta") {
          c.partial = str(c.partial) + str(delta.partial_json);
          try {
            c.input = JSON.parse(str(c.partial));
          } catch {
            c.input = c.partial;
          }
        } else if (delta.type !== "signature_delta") {
          fallback();
          continue;
        }
        block(c, "assistant", key, line);
      } else if (
        ![
          "message_delta",
          "message_stop",
          "content_block_stop",
          "ping",
        ].includes(eventType)
      )
        fallback();
      continue;
    }
    if (type === "assistant" || type === "user") {
      const message = obj(v.message),
        content = message.content;
      const id = str(v.uuid) || `offset:${line.start}`;
      function finalBlock(value: unknown, n: number) {
        const c = obj(value);
        // Claude emits individual finalized blocks sharing message.id. Match a
        // streamed block when available; message.id alone is not a block ID.
        const streamed = [...streams].find(
          ([key, partial]) =>
            key.startsWith(`claude:${str(message.id)}:`) &&
            !finalized.has(key) &&
            partial.type === c.type &&
            (c.type === "tool_use"
              ? partial.id === c.id
              : c.type === "text"
                ? str(c.text).startsWith(str(partial.text))
                : c.type === "thinking" &&
                  str(c.thinking).startsWith(str(partial.thinking))),
        );
        const key = streamed?.[0] ?? `claude:${id}:${n}`;
        if (streamed) finalized.add(key);
        block(c, type, key, line);
      }
      if (typeof content === "string")
        finalBlock({ type: "text", text: content }, 0);
      else if (Array.isArray(content) && content.length <= 100)
        content.forEach(finalBlock);
      else fallback();
      continue;
    }
    const item = obj(v.item);
    if (Object.keys(item).length) {
      const key = `codex:${str(item.id) || line.start}`;
      if (item.type === "error") {
        put(key, line, {
          kind: "event",
          label: "Error",
          body: str(item.message) || pretty(item),
          error: true,
        });
      } else if (item.type === "agent_message" || item.type === "reasoning") {
        if (str(item.text).trim())
          put(key, line, {
            kind: item.type === "reasoning" ? "thinking" : "message",
            label: item.type === "reasoning" ? "Thinking" : "Assistant",
            body: str(item.text),
          });
      } else if (
        item.type === "command_execution" ||
        item.type === "mcp_tool_call"
      ) {
        const command = item.type === "command_execution";
        const error =
          !!item.error ||
          (typeof item.exit_code === "number" && item.exit_code !== 0) ||
          item.status === "failed";
        const complete =
          type === "item.completed" || item.status === "completed";
        put(
          key,
          line,
          {
            kind: "tool",
            label: command
              ? `Command · ${str(item.command).slice(0, 160)}`
              : `Tool · ${str(item.server)}.${str(item.tool)}`,
            input: pretty(command ? item.command : item.arguments),
            body: pretty(
              command ? item.aggregated_output : (item.error ?? item.result),
            ),
            status: error
              ? "Failed"
              : complete
                ? "Completed"
                : item.status === "in_progress" || type === "item.started"
                  ? "Started"
                  : "Status unavailable",
            error,
          },
          complete || error,
        );
      } else fallback();
      continue;
    }
    if (type === "error" || type === "turn.failed") {
      put(`error:${line.start}`, line, {
        kind: "event",
        label: "Error",
        body: str(obj(v.error).message) || str(v.message) || pretty(v),
        error: true,
      });
      continue;
    }
    if (type === "result") {
      const body = str(v.result);
      const summary = [
        typeof v.num_turns === "number" ? `${v.num_turns} turns` : "",
        typeof v.duration_ms === "number" ? duration(v.duration_ms) : "",
        typeof v.total_cost_usd === "number"
          ? "$" + v.total_cost_usd.toFixed(2)
          : "",
      ].filter(Boolean);
      if (!v.is_error && summary.length)
        put(`system:${line.start}`, line, {
          kind: "system",
          label: "Finished",
          body: summary.join(" · "),
        });
      if (v.is_error)
        put(`error:${line.start}`, line, {
          kind: "event",
          label: "Failed",
          body: body || contentText(v.errors) || pretty(v),
          error: true,
        });
      else if (
        body &&
        ![...entries.values()].some(
          (e) =>
            e.kind === "message" && e.label === "Assistant" && e.body === body,
        )
      )
        put(`result:${line.start}`, line, {
          kind: "message",
          label: "Assistant",
          body,
        });
      continue;
    }
    // Heartbeats carry transport timing, not conversational content. Keep
    // unrecognized progress and explicit errors inspectable in the timeline.
    if (
      type === "tool_progress" &&
      v.heartbeat === true &&
      !v.error &&
      !v.is_error
    )
      continue;
    if (type === "rate_limit_event") {
      const info = obj(v.rate_limit_info);
      if (info.status === "rejected")
        put(`error:${line.start}`, line, {
          kind: "event",
          label: "Rate limit reached",
          body: pretty(info),
          error: true,
        });
      continue;
    }
    if (type === "system" && !v.error && !v.is_error) {
      const s = systemEvent(v);
      if (s) put(`system:${line.start}`, line, { kind: "system", ...s });
      continue;
    }
    if (type === "thread.started" && !v.error) {
      put(`system:${line.start}`, line, {
        kind: "system",
        label: "Session started",
        body: str(v.thread_id) ? `thread ${str(v.thread_id)}` : "",
      });
      continue;
    }
    if (type === "turn.completed" && !v.error) {
      const u = obj(v.usage);
      const n = (k: string) =>
        typeof u[k] === "number" ? (u[k] as number) : undefined;
      const input = n("input_tokens"),
        output = n("output_tokens");
      put(`system:${line.start}`, line, {
        kind: "system",
        label: "Turn complete",
        body:
          input !== undefined || output !== undefined
            ? `${count(input ?? 0)} in · ${count(output ?? 0)} out tokens`
            : "",
      });
      continue;
    }
    if (["turn.started", "ping"].includes(type) && !v.error && !v.is_error)
      continue;
    fallback();
  }
  return [...entries.values()]
    .filter((e) => e.body || e.kind === "tool" || e.kind === "system")
    .sort((a, b) => a.offset - b.offset);
}
