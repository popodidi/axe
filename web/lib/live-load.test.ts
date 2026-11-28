import { describe, expect, test } from "vitest";
import { LiveLoad } from "./live-load";

// A load the test resolves by hand.
function manual() {
  const pending: { resolve: (v: number) => void; signal: AbortSignal }[] = [];
  const load = (signal: AbortSignal) =>
    new Promise<number>((resolve) => pending.push({ resolve, signal }));
  return { pending, load };
}
const flush = () => new Promise((r) => setTimeout(r, 0));

describe("LiveLoad", () => {
  test("changes during a read queue one more read and never abort it", async () => {
    const { pending, load } = manual();
    const got: number[] = [];
    const l = new LiveLoad(load, (v) => got.push(v), () => {});
    l.refresh();
    // A burst of server changes while the first read is slow.
    for (let i = 0; i < 20; i++) l.refresh();
    expect(pending.length).toBe(1);
    expect(pending[0].signal.aborted).toBe(false);
    pending[0].resolve(1);
    await flush();
    expect(got).toEqual([1]);
    // The burst collapses into one follow-up read.
    expect(pending.length).toBe(2);
    pending[1].resolve(2);
    await flush();
    expect(got).toEqual([1, 2]);
    expect(pending.length).toBe(2);
  });
  test("stop aborts the read in flight and drops its result", async () => {
    const { pending, load } = manual();
    const got: number[] = [];
    const errors: unknown[] = [];
    const l = new LiveLoad(load, (v) => got.push(v), (e) => errors.push(e));
    l.refresh();
    l.refresh();
    l.stop();
    expect(pending[0].signal.aborted).toBe(true);
    pending[0].resolve(1);
    await flush();
    l.refresh();
    expect(got).toEqual([]);
    expect(pending.length).toBe(1);
    expect(errors).toEqual([]);
  });
  test("a failed read reports and still runs the queued one", async () => {
    let n = 0;
    const errors: string[] = [];
    const got: number[] = [];
    const l = new LiveLoad(
      async () => {
        n++;
        if (n === 1) throw new Error("HTTP 502");
        return n;
      },
      (v) => got.push(v),
      (e) => errors.push(String(e)),
    );
    l.refresh();
    l.refresh();
    await flush();
    await flush();
    expect(errors).toEqual(["Error: HTTP 502"]);
    expect(got).toEqual([2]);
  });
});
