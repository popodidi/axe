import type { Page } from "@playwright/test";
import { mockDesk, runs } from "./fixtures";
export const markdown =
  "## Review the change\n\nPlease check **readability** and [documentation](https://example.com).\n\n- Keep the timeline compact\n- Preserve tool output\n\n```js\nconst safe = true;\n```\n\n<script>window.injected=true</script>\n\n[unsafe](javascript:alert(1))";
export const answer =
  "## Result\n\nThe transcript is readable.\n\n1. Messages retain their formatting.\n2. Tool details stay available.\n\n```sh\nnpm test\n```";
export async function conversationFixture(page: Page, agent = "claude") {
  await mockDesk(page);
  const values: unknown[] = [
    {
      type: "system",
      subtype: "init",
      diagnostic: "routine diagnostic metadata",
    },
    {
      type: "user",
      uuid: "u1",
      message: { content: [{ type: "text", text: markdown }] },
    },
  ];
  if (agent === "claude")
    values.push(
      {
        type: "tool_progress",
        heartbeat: true,
        parent_tool_use_id: "t1",
        elapsed_time_seconds: 30,
      },
      {
        type: "assistant",
        message: {
          id: "m1",
          content: [
            {
              type: "thinking",
              thinking: "",
              signature: "opaque-signature".repeat(300),
            },
          ],
        },
      },
      {
        type: "assistant",
        message: {
          id: "m2",
          content: [
            {
              type: "thinking",
              thinking: "Check the recorded test results before summarizing.",
            },
          ],
        },
      },
      {
        type: "assistant",
        message: {
          id: "m2",
          content: [
            {
              type: "tool_use",
              id: "t1",
              name: "Bash",
              input: {
                command: "npm test",
                description: "Run the dashboard tests",
              },
            },
          ],
        },
      },
      { type: "rate_limit_event", rate_limit_info: { status: "allowed" } },
      {
        type: "user",
        message: {
          content: [
            {
              type: "tool_result",
              tool_use_id: "t1",
              content:
                "Tests passed\n" +
                "long-command-output-".repeat(80) +
                "\n" +
                "output line\n".repeat(100),
            },
          ],
        },
      },
      {
        type: "assistant",
        message: { id: "m3", content: [{ type: "text", text: answer }] },
      },
    );
  else
    values.push(
      {
        type: "item.completed",
        item: {
          id: "r",
          type: "reasoning",
          text: "Check the recorded test results before summarizing.",
        },
      },
      {
        type: "item.started",
        item: { id: "t1", type: "command_execution", command: "npm test" },
      },
      {
        type: "item.completed",
        item: {
          id: "t1",
          type: "command_execution",
          command: "npm test",
          aggregated_output:
            "Tests passed\n" +
            "long-command-output-".repeat(80) +
            "\n" +
            "output line\n".repeat(100),
          exit_code: 0,
        },
      },
      {
        type: "item.completed",
        item: { id: "a", type: "agent_message", text: answer },
      },
    );
  values.push(
    agent === "codex"
      ? {
          type: "item.completed",
          item: {
            id: "error1",
            type: "error",
            message: "Example actionable error: credentials expired",
          },
        }
      : {
          type: "error",
          message: "Example actionable error: credentials expired",
        },
    { type: "future_event", description: "Unknown event preserved" },
  );
  let offset = 0;
  const lines = values.map((value) => {
    const text = JSON.stringify(value),
      start = offset;
    offset += new TextEncoder().encode(text).length + 1;
    return { text, start, end: offset, fragment: false };
  });
  await page.route("http://axe.test/api/runs/*/log?*", (route) =>
    route.fulfill({
      json: {
        run: {
          ...runs[0],
          agent,
          ended_at: "2026-10-28T08:00:00Z",
          exit_code: 0,
        },
        page: {
          generation: "conversation",
          start: 0,
          end: offset,
          size: offset,
          lines,
        },
      },
    }),
  );
}
