"use client";
import { useCallback, useEffect, useLayoutEffect, useRef } from "react";
type ScrollParam = "listY" | "detailY";
// Browsers rate-limit history writes (Firefox warns after ~200 in 10s), so
// scroll offsets are written at most once per interval. Navigation inside the
// app snapshots offsets synchronously, so a pending write never loses state.
export const SCROLL_WRITE_MS = 250;
function setOffset(url: URL, param: ScrollParam, offset: number) {
  const y = Math.max(0, Math.round(offset));
  if (y) url.searchParams.set(param, String(y));
  else url.searchParams.delete(param);
}
// Capture before push so even a scroll immediately followed by navigation is saved.
export function snapshotPaneScroll(url: URL) {
  document
    .querySelectorAll<HTMLElement>('[data-scroll-ready="true"]')
    .forEach((el) => {
      if (el.clientHeight)
        setOffset(url, el.dataset.scrollParam as ScrollParam, el.scrollTop);
    });
}
function restorationKey(query: string, param: ScrollParam) {
  const p = new URLSearchParams(query);
  p.delete(param === "listY" ? "detailY" : "listY");
  return p.toString();
}
export function usePaneScroll(
  query: string,
  param: ScrollParam,
  ready: boolean,
  focusId?: string,
) {
  const ref = useRef<HTMLElement>(null);
  const applied = useRef<string | null>(null);
  useLayoutEffect(() => {
    const pane = ref.current;
    if (!ready || !pane) {
      applied.current = null;
      return;
    }
    if (!pane.clientHeight) return; // The mobile list is hidden while detail is open.
    const key = restorationKey(query, param);
    if (applied.current === key) return;
    const value = Number(new URLSearchParams(query).get(param));
    pane.scrollTop = Number.isFinite(value) ? Math.max(0, value) : 0;
    if (focusId)
      pane
        .querySelector<HTMLElement>("#" + CSS.escape(focusId))
        ?.scrollIntoView({ block: "nearest" });
    applied.current = key;
  }, [query, param, ready, focusId]);
  const timer = useRef<number | undefined>(undefined);
  const lastWrite = useRef(0);
  const write = useCallback(() => {
    timer.current = undefined;
    lastWrite.current = Date.now();
    const pane = ref.current;
    if (!ready || !pane?.clientHeight) return;
    const url = new URL(location.href);
    // Ignore events queued by the previous screen while this one is loading.
    if (applied.current !== restorationKey(url.search, param)) return;
    setOffset(url, param, pane.scrollTop);
    applied.current = restorationKey(url.search, param);
    if (url.href !== location.href)
      history.replaceState(history.state, "", url);
  }, [param, ready]);
  const onScroll = useCallback(() => {
    if (timer.current !== undefined) return;
    const wait = SCROLL_WRITE_MS - (Date.now() - lastWrite.current);
    if (wait <= 0) write();
    else timer.current = window.setTimeout(write, wait);
  }, [write]);
  useEffect(() => {
    // A write pending from the old entry must not land on the Back/Forward target.
    const cancel = () => {
      window.clearTimeout(timer.current);
      timer.current = undefined;
    };
    window.addEventListener("popstate", cancel);
    return () => {
      cancel();
      window.removeEventListener("popstate", cancel);
    };
  }, []);
  return {
    ref,
    onScroll,
    "data-scroll-param": param,
    "data-scroll-ready": ready,
  };
}
