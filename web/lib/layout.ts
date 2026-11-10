import { useCallback, useSyncExternalStore } from "react";
import { layoutKey, paneVars, type Pane } from "./layout-script";
import { motionMs } from "./motion";
let navMotion: ReturnType<typeof setTimeout> | undefined;
type Saved = {
  nav?: "collapsed";
  detail?: number;
  routine?: number;
  navWidth?: number;
};
const event = "harvis-layout";
function read(): Saved {
  try {
    const saved = JSON.parse(localStorage.getItem(layoutKey) || "{}");
    return saved && typeof saved === "object" ? saved : {};
  } catch {
    return {};
  }
}
function save(patch: Saved) {
  const next = { ...read(), ...patch };
  (Object.keys(next) as (keyof Saved)[]).forEach(
    (k) => next[k] === undefined && delete next[k],
  );
  try {
    localStorage.setItem(layoutKey, JSON.stringify(next));
  } catch {
    // Storage may be unavailable (private mode); the layout still applies.
  }
}
function subscribe(fn: () => void) {
  window.addEventListener(event, fn);
  return () => window.removeEventListener(event, fn);
}
const isCollapsed = () => document.documentElement.dataset.nav === "collapsed";
export function useNavCollapsed(): [boolean, () => void] {
  const collapsed = useSyncExternalStore(subscribe, isCollapsed, () => false);
  const toggle = useCallback(() => {
    const next = !isCollapsed();
    const root = document.documentElement;
    root.dataset.navMotion = "";
    clearTimeout(navMotion);
    navMotion = setTimeout(
      () => delete root.dataset.navMotion,
      motionMs("--duration-layout"),
    );
    if (next) document.documentElement.dataset.nav = "collapsed";
    else delete document.documentElement.dataset.nav;
    save({ nav: next ? "collapsed" : undefined });
    window.dispatchEvent(new Event(event));
  }, []);
  return [collapsed, toggle];
}
// Applies a pane width immediately; persist saves it for the next visit.
// null restores the pane's default width.
export function setPaneWidth(pane: Pane, px: number | null, persist = true) {
  const root = document.documentElement.style;
  if (px === null) root.removeProperty(paneVars[pane]);
  else root.setProperty(paneVars[pane], px + "px");
  if (persist)
    save({ [pane === "nav" ? "navWidth" : pane]: px ?? undefined });
}
export const setDetailWidth = (px: number | null, persist = true) =>
  setPaneWidth("detail", px, persist);
