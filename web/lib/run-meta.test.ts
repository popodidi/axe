import { expect, test } from "vitest";
import {
  clock,
  count,
  duration,
  runtimeOf,
  size,
  usageTotals,
  type RunMeta,
} from "./run-meta";

test("runtime facts prefer the harness's init record, then the start event", () => {
  const meta: RunMeta = {
    start: {
      runtime: "claude-1",
      harness: "claude-code",
      rule: "rule:1 (default)",
      use: "claude-code:claude-opus-5-5:high",
      cmd: ["claude", "-p", "--model", "ignored"],
    },
    files: {
      first_line: JSON.stringify({
        type: "system",
        subtype: "init",
        model: "claude-opus-5-5[1m]",
        claude_code_version: "2.1.325",
        permissionMode: "bypassPermissions",
      }),
    },
  };
  expect(runtimeOf(meta)).toEqual({
    runtime: "claude-1",
    harness: "claude-code",
    model: "claude-opus-5-5[1m]",
    effort: "high",
    version: "2.1.325",
    permission: "bypassPermissions",
    rule: "rule:1 (default)",
  });
});

test("runtime facts fall back to the command, and stay empty when unrecorded", () => {
  const codex: RunMeta = {
    start: {
      agent: "codex-1",
      cmd: [
        "codex",
        "exec",
        "-m",
        "gpt-x",
        "-c",
        'model_reasoning_effort="low"',
      ],
    },
    files: { first_line: "Reading additional input from stdin..." },
  };
  expect(runtimeOf(codex)).toMatchObject({
    runtime: "codex-1",
    model: "gpt-x",
    effort: "low",
    version: "",
  });
  expect(runtimeOf(undefined, "claude")).toMatchObject({
    runtime: "claude",
    model: "",
    effort: "",
  });
});

test("formats keep a stable, short shape", () => {
  const t = new Date(2026, 10, 25, 9, 5, 7).getTime();
  expect(clock(t)).toBe("09:05:07");
  expect(clock(t, t)).toBe("09:05:07");
  expect(clock(t, t - 86_400_000)).toMatch(/25.* 09:05:07$/);
  expect(duration(850)).toBe("850ms");
  expect(duration(12_400)).toBe("12s");
  expect(duration(185_000)).toBe("3m 05s");
  expect(duration(3_720_000)).toBe("1h 02m");
  expect(duration(-1)).toBe("");
  expect(count(950)).toBe("950");
  expect(count(1_250)).toBe("1.3k");
  expect(count(34_500)).toBe("35k");
  expect(count(1_200_000)).toBe("1.2M");
  expect(size(512)).toBe("512 B");
  expect(size(12 * 1024)).toBe("12 KB");
  expect(size(1.5 * 1024 * 1024)).toBe("1.5 MB");
});

test("usage totals sum models and are absent when unrecorded", () => {
  expect(usageTotals({ usage: null })).toBeUndefined();
  const m = { cache_read: 1, cache_write: 2, cost: 0.5 };
  expect(
    usageTotals({
      usage: {
        status: "known",
        models: [
          { model: "a", input: 10, output: 5, ...m },
          { model: "b", input: 1, output: 1, ...m },
        ],
      },
    }),
  ).toEqual({ input: 11, output: 6, cache_read: 2, cache_write: 4, cost: 1 });
});
