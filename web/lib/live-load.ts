import { useEffect, useRef, useState } from "react";

// A read that follows server changes (task 2044). A change while a read is in
// flight queues one more read after it instead of aborting it: the server
// reports a change a second or more while runs are live, so a restart on
// every change never finishes on a slow link (a phone over 4G) and the pane
// stays on its skeleton. Only `stop` (the subject changed or unmounted)
// aborts.
export class LiveLoad<T> {
  private busy = false;
  private again = false;
  private readonly c = new AbortController();
  constructor(
    private readonly load: (signal: AbortSignal) => Promise<T>,
    private readonly done: (value: T) => void,
    private readonly fail: (error: unknown) => void,
  ) {}
  /** Reads now, or once more after the read in flight. */
  refresh() {
    if (this.c.signal.aborted) return;
    if (this.busy) {
      this.again = true;
      return;
    }
    this.busy = true;
    this.load(this.c.signal)
      .then(
        (v) => {
          if (!this.c.signal.aborted) this.done(v);
        },
        (e) => {
          if (!this.c.signal.aborted) this.fail(e);
        },
      )
      .finally(() => {
        this.busy = false;
        if (this.again) {
          this.again = false;
          this.refresh();
        }
      });
  }
  stop() {
    this.c.abort();
  }
}

export type Live<T> = { value: T | null; error: string; refresh: () => void };

/**
 * `load` for subject `key`, again after each `revision` (coalesced, see
 * [`LiveLoad`]). A new key aborts the old read; the last value stays shown
 * until the new one arrives. Nothing loads while `enabled` is false.
 */
export function useLive<T>(
  load: (signal: AbortSignal) => Promise<T>,
  key: string,
  revision: number,
  enabled = true,
): Live<T> {
  const [state, setState] = useState<{ value: T | null; error: string }>({
    value: null,
    error: "",
  });
  const loadRef = useRef(load);
  const live = useRef<LiveLoad<T> | null>(null);
  const seen = useRef(revision);
  useEffect(() => {
    loadRef.current = load;
  });
  useEffect(() => {
    if (!enabled) return;
    const l = new LiveLoad<T>(
      (s) => loadRef.current(s),
      (value) => setState({ value, error: "" }),
      (e) => setState((s) => ({ ...s, error: String(e) })),
    );
    live.current = l;
    seen.current = revision;
    l.refresh();
    return () => {
      l.stop();
      live.current = null;
    };
    // The revision effect below refreshes; a new revision is not a new subject.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, enabled]);
  useEffect(() => {
    if (revision === seen.current) return;
    seen.current = revision;
    live.current?.refresh();
  }, [revision]);
  const [refresh] = useState(() => () => live.current?.refresh());
  return { ...state, refresh };
}
