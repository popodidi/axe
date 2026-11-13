import { useCallback, useSyncExternalStore } from "react";
// Which linked tasks the goal page lists: open ones (the default) or all of
// them. The choice is stored per browser.
export type GoalTaskFilter = "open" | "all";
export const goalTaskFilterKey = "axe-goal-tasks";
const event = "axe-goal-tasks";
// Used when storage is unavailable (private mode), so the switch still works.
let memory: GoalTaskFilter = "open";
function subscribe(fn: () => void) {
  window.addEventListener(event, fn);
  window.addEventListener("storage", fn);
  return () => {
    window.removeEventListener(event, fn);
    window.removeEventListener("storage", fn);
  };
}
function read(): GoalTaskFilter {
  try {
    const v = localStorage.getItem(goalTaskFilterKey);
    return v === "all" || v === "open" ? v : memory;
  } catch {
    return memory;
  }
}
export function useGoalTaskFilter(): [GoalTaskFilter, (f: GoalTaskFilter) => void] {
  const filter = useSyncExternalStore(subscribe, read, () => "open" as const);
  const set = useCallback((f: GoalTaskFilter) => {
    memory = f;
    try {
      localStorage.setItem(goalTaskFilterKey, f);
    } catch {
      // Storage may be unavailable; the in-memory choice still applies.
    }
    window.dispatchEvent(new Event(event));
  }, []);
  return [filter, set];
}
