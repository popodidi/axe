import { useCallback, useSyncExternalStore } from "react";
// How a run's prompt shows in the run details: Markdown rendered (the
// default) or the raw text. Stored per browser and per prompt, so the
// choice carries to every run.
export type PromptView = "rendered" | "raw";
export type PromptKind = "system" | "user";
export const promptViewKey = (kind: PromptKind) => `axe-prompt-view-${kind}`;
const event = "axe-prompt-view";
// Used when storage is unavailable (private mode), so the switch still works.
const memory: Record<PromptKind, PromptView> = {
  system: "rendered",
  user: "rendered",
};
function subscribe(fn: () => void) {
  window.addEventListener(event, fn);
  window.addEventListener("storage", fn);
  return () => {
    window.removeEventListener(event, fn);
    window.removeEventListener("storage", fn);
  };
}
function read(kind: PromptKind): PromptView {
  try {
    const v = localStorage.getItem(promptViewKey(kind));
    return v === "raw" || v === "rendered" ? v : memory[kind];
  } catch {
    return memory[kind];
  }
}
export function usePromptView(
  kind: PromptKind,
): [PromptView, (v: PromptView) => void] {
  const view = useSyncExternalStore(
    subscribe,
    () => read(kind),
    () => "rendered" as const,
  );
  const set = useCallback(
    (v: PromptView) => {
      memory[kind] = v;
      try {
        localStorage.setItem(promptViewKey(kind), v);
      } catch {
        // Storage may be unavailable; the in-memory choice still applies.
      }
      window.dispatchEvent(new Event(event));
    },
    [kind],
  );
  return [view, set];
}
