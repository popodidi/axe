import { expect, test } from "vitest";
import { buildTranscript } from "./transcript";
import type { LogLine } from "./run-log";
export function lines(values: unknown[]): LogLine[] {
  let offset = 0;
  return values.map((value) => {
    const text = JSON.stringify(value),
      start = offset;
    offset += text.length + 1;
    return { text, start, end: offset, fragment: false };
  });
}
const message = (content: unknown[], id = "m", type = "assistant") => ({
  type,
  message: { id, content },
});
const event = (value: unknown) => ({ type: "stream_event", event: value });
test("Claude partial text reconciles with final snapshot; identical distinct messages remain", () => {
  const records = lines([
    event({ type: "message_start", message: { id: "m" } }),
    event({
      type: "content_block_start",
      index: 0,
      content_block: { type: "text", text: "" },
    }),
    event({
      type: "content_block_delta",
      index: 0,
      delta: { type: "text_delta", text: "Hello **" },
    }),
    event({
      type: "content_block_delta",
      index: 0,
      delta: { type: "text_delta", text: "world**" },
    }),
    message([{ type: "text", text: "Hello **world**" }]),
    message([{ type: "text", text: "Hello **world**" }], "m2"),
  ]);
  expect(buildTranscript(records.slice(0, 3)).map((e) => e.body)).toEqual([
    "Hello **",
  ]);
  expect(buildTranscript(records).map((e) => e.body)).toEqual([
    "Hello **world**",
    "Hello **world**",
  ]);
  expect(buildTranscript(records)[0].offset).toBe(records[1].start);
});
test("Claude associates tools across history pages, preserves order and hides opaque/routine data", () => {
  const records = lines([
    { type: "system", subtype: "init", secret: "metadata" },
    message([{ type: "thinking", thinking: "", signature: "opaque" }]),
    message([{ type: "text", text: "# Request" }], "u", "user"),
    message(
      [
        {
          type: "tool_use",
          id: "t",
          name: "Bash",
          input: { command: "echo hello" },
        },
      ],
      "call",
    ),
    message([{ type: "text", text: "Working" }], "progress"),
    message(
      [
        {
          type: "tool_result",
          tool_use_id: "t",
          content: "long output\n".repeat(300),
          is_error: true,
        },
      ],
      "result",
      "user",
    ),
    { type: "rate_limit_event", rate_limit_info: { status: "allowed" } },
    { type: "future", data: "retained" },
  ]);
  const tail = buildTranscript(records.slice(5));
  expect(tail[0].label).toContain("outside loaded history");
  const all = buildTranscript(records);
  expect(all.map((e) => e.kind)).toEqual([
    "message",
    "tool",
    "message",
    "event",
  ]);
  expect(all[1]).toMatchObject({
    label: "Bash · echo hello",
    status: "Failed",
    error: true,
    offset: records[3].start,
  });
  expect(all[1].body).toBe("long output\n".repeat(300));
  expect(JSON.stringify(all)).not.toMatch(/opaque|metadata|allowed/);
  expect(all[3].body).toContain("retained");
});
test("Codex snapshots merge by id with honest status and available reasoning", () => {
  const records = lines([
    { type: "thread.started", thread_id: "x" },
    {
      type: "item.started",
      item: { type: "command_execution", id: "c", command: "ls" },
    },
    {
      type: "item.updated",
      item: { type: "agent_message", id: "a", text: "partial" },
    },
    {
      type: "item.completed",
      item: {
        type: "command_execution",
        id: "c",
        command: "ls",
        aggregated_output: "failed",
        exit_code: 2,
      },
    },
    {
      type: "item.completed",
      item: { type: "agent_message", id: "a", text: "final" },
    },
    {
      type: "item.completed",
      item: { type: "reasoning", id: "r", text: "Available summary" },
    },
    {
      type: "item.completed",
      item: {
        type: "mcp_tool_call",
        id: "m",
        server: "docs",
        tool: "read",
        arguments: {},
        result: { text: "ok" },
      },
    },
    { type: "turn.failed", error: { message: "Authentication required" } },
  ]);
  expect(buildTranscript(records.slice(0, 2))[0].status).toBe("Started");
  const all = buildTranscript(records);
  expect(all.map((e) => e.kind)).toEqual([
    "tool",
    "message",
    "thinking",
    "tool",
    "event",
  ]);
  expect(all[0]).toMatchObject({
    error: true,
    status: "Failed",
    body: "failed",
  });
  expect(all[1].body).toBe("final");
  expect(all[4].error).toBe(true);
});
test("errors and unknown records remain visible; signature-only payloads do not become reasoning", () => {
  const all = buildTranscript(
    lines([
      message([
        { type: "thinking", thinking: "Available text", signature: "opaque" },
      ]),
      {
        type: "rate_limit_event",
        rate_limit_info: { status: "rejected", resetsAt: 123 },
      },
      { type: "error", message: "Fix credentials" },
      { type: "result", is_error: true, errors: ["failed task"] },
      { type: "new_protocol", message: "keep me" },
    ]),
  );
  expect(all[0].body).toBe("Available text");
  expect(all.slice(1, 4).every((e) => e.error)).toBe(true);
  expect(all[4].body).toContain("keep me");
});
test("Claude finalized blocks sharing a message ID must not overwrite one another", () => {
  const all = buildTranscript(
    lines([
      message([{ type: "text", text: "First paragraph" }]),
      message([{ type: "thinking", thinking: "Available thinking" }]),
      message([{ type: "text", text: "Second paragraph" }]),
    ]),
  );
  expect(all.map((e) => e.body)).toEqual([
    "First paragraph",
    "Available thinking",
    "Second paragraph",
  ]);
});
test("signature-only final thinking preserves available streamed text", () => {
  const all = buildTranscript(
    lines([
      event({ type: "message_start", message: { id: "m" } }),
      event({
        type: "content_block_start",
        index: 0,
        content_block: { type: "thinking", thinking: "Observed summary" },
      }),
      message([{ type: "thinking", thinking: "", signature: "opaque" }]),
    ]),
  );
  expect(all.map((e) => e.body)).toEqual(["Observed summary"]);
});
test("Claude heartbeat progress stays raw-only without hiding unknown progress or errors", () => {
  const records = lines([
    {
      type: "tool_progress",
      heartbeat: true,
      parent_tool_use_id: "t",
      elapsed_time_seconds: 30,
    },
    { type: "tool_progress", message: "Unknown progress content" },
    { type: "tool_progress", heartbeat: true, error: "Transport failed" },
  ]);
  const original = records.map((record) => record.text);
  const entries = buildTranscript(records);
  expect(entries).toHaveLength(2);
  expect(entries[0].body).toContain("Unknown progress content");
  expect(entries[1].error).toBe(true);
  expect(records.map((record) => record.text)).toEqual(original);
});
test("Codex error items surface their actionable message without asserting run failure", () => {
  const entries = buildTranscript(
    lines([
      {
        type: "item.completed",
        item: {
          id: "e",
          type: "error",
          message: "Credentials expired; sign in again",
        },
      },
    ]),
  );
  expect(entries).toHaveLength(1);
  expect(entries[0]).toMatchObject({
    kind: "event",
    label: "Error",
    body: "Credentials expired; sign in again",
    error: true,
  });
  expect(entries[0].status).toBeUndefined();
});
