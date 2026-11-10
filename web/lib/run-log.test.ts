import { expect, test } from "vitest";
import { formatLog, logSize } from "./run-log";
const line = (value: unknown) => ({
  start: 0,
  end: 100,
  text: JSON.stringify(value),
  fragment: false,
});
test("unknown events, errors, fragments and plain output remain inspectable", () => {
  expect(formatLog(line({ type: "future", data: "kept" }))[0]).toMatchObject({
    label: "future",
    body: expect.stringContaining("kept"),
  });
  expect(
    formatLog(
      line({
        type: "item.completed",
        item: { type: "future_tool", output: "kept" },
      }),
    )[0].label,
  ).toBe("future_tool");
  expect(
    formatLog(
      line({ type: "turn.failed", error: { message: "credentials" } }),
    )[0].error,
  ).toBe(true);
  expect(
    formatLog({ ...line(null), text: "<script>alert(1)</script>" })[0].body,
  ).toBe("<script>alert(1)</script>");
  expect(formatLog({ ...line({}), fragment: true })[0].label).toContain(
    "fragment",
  );
  expect(formatLog(line(null))[0].body).toBe("null");
  expect(logSize([line("abc")])).toBe(10);
});
