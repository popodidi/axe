import { useCallback, useSyncExternalStore } from "react";
// Whether the contents rail (components/ui/toc.tsx) is pinned open. Collapsed
// (a column of ticks) by default; the choice is stored per browser and shared
// by every rail, so pinning on one page pins it everywhere.
export const tocPinKey = "axe-toc-pinned";
const event = "axe-toc-pinned";
// Used when storage is unavailable (private mode), so the toggle still works.
let memory = false;
function subscribe(fn: () => void) {
  window.addEventListener(event, fn);
  window.addEventListener("storage", fn);
  return () => {
    window.removeEventListener(event, fn);
    window.removeEventListener("storage", fn);
  };
}
export function readTocPinned(): boolean {
  try {
    const v = localStorage.getItem(tocPinKey);
    return v === null ? memory : v === "1";
  } catch {
    return memory;
  }
}
export function writeTocPinned(pinned: boolean) {
  memory = pinned;
  try {
    if (pinned) localStorage.setItem(tocPinKey, "1");
    else localStorage.removeItem(tocPinKey);
  } catch {
    // Storage may be unavailable; the in-memory choice still applies.
  }
  window.dispatchEvent(new Event(event));
}
export function useTocPinned(): [boolean, (pinned: boolean) => void] {
  const pinned = useSyncExternalStore(subscribe, readTocPinned, () => false);
  return [pinned, useCallback((p: boolean) => writeTocPinned(p), [])];
}
