"use client";
// The one table of contents (the doc page, rendered run prompts): a compact
// numbered list of a body's headings with the section in view highlighted.
// `TocRail` is the sticky side list, `TocFold` the "Contents" disclosure for
// narrow space; the consumer's CSS shows one of them. The rail is collapsed
// to a column of ticks, one per heading, until hovered or focused (the list
// then opens over the content) or pinned open (lib/toc-pin.ts); a consumer
// widens its column for a pinned rail with `:has([data-pinned])`. Entries come from
// lib/doc-toc.ts (`tocEntries`); a short body has none, and both render
// nothing.
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type FocusEvent,
  type MouseEvent,
} from "react";
import { activeHeading, TOC_LEVELS, type TocEntry, type TocHeading } from "../../lib/doc-toc";
import { useTocPinned } from "../../lib/toc-pin";
import { cx } from "./cx";
import styles from "./toc.module.css";

type TocProps = {
  entries: TocEntry[];
  active: string;
  // Jump to a heading without following the link (a frame, a scroll box).
  onJump?: (id: string) => void;
};

function TocList({ entries, active, onJump, follow = false }: TocProps & { follow?: boolean }) {
  const list = useRef<HTMLOListElement>(null);
  // A rail taller than its box keeps the current entry in view.
  useLayoutEffect(() => {
    const ol = list.current;
    const rail = follow ? ol?.closest<HTMLElement>(`.${styles.rail}`) : null;
    const a = ol?.querySelector<HTMLElement>("a[aria-current]");
    if (!rail || !a || rail.scrollHeight <= rail.clientHeight) return;
    const [r, l] = [rail.getBoundingClientRect(), a.getBoundingClientRect()];
    if (l.top < r.top) rail.scrollTop -= r.top - l.top;
    else if (l.bottom > r.bottom) rail.scrollTop += l.bottom - r.bottom;
  }, [active, follow]);
  const click = (e: MouseEvent<HTMLAnchorElement>, id: string) => {
    if (!onJump || e.metaKey || e.ctrlKey || e.shiftKey || e.button) return;
    e.preventDefault();
    onJump(id);
  };
  return (
    <nav aria-label="Contents">
      <ol ref={list} className={styles.list}>
        {entries.map((h) => (
          <li key={h.id}>
            <a
              href={`#${encodeURIComponent(h.id)}`}
              data-depth={h.depth}
              title={h.text}
              className={cx(h.id === active && styles.active)}
              aria-current={h.id === active ? "location" : undefined}
              onClick={(e) => click(e, h.id)}
            >
              <span className={styles.tick} aria-hidden="true" />
              {h.number && (
                <span className={styles.number} aria-hidden="true">
                  {h.depth ? h.number : `${h.number}.`}
                </span>
              )}
              <span className={styles.text}>{h.text}</span>
            </a>
          </li>
        ))}
      </ol>
    </nav>
  );
}

// The sticky side list. Collapsed it is a column of ticks; pointer hover or
// keyboard focus opens the full list over the content (Escape closes it),
// and the pin button keeps it open in the page's flow.
export function TocRail({ className, ...props }: TocProps & { className?: string }) {
  const [pinned, setPinned] = useTocPinned();
  const [hover, setHover] = useState(false);
  const [focus, setFocus] = useState(false);
  const open = !pinned && (hover || focus) && props.entries.length > 0;
  // Escape closes the open list wherever focus is (a hovered rail need not
  // hold it); focus stays where it was.
  useEffect(() => {
    if (!open) return;
    const key = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      setHover(false);
      setFocus(false);
    };
    document.addEventListener("keydown", key, true);
    return () => document.removeEventListener("keydown", key, true);
  }, [open]);
  if (!props.entries.length) return null;
  // Keyboard focus only: a clicked entry keeps focus, and the list should
  // still close when the pointer leaves.
  const focusIn = (e: FocusEvent<HTMLDivElement>) => {
    if (e.target.matches(":focus-visible")) setFocus(true);
  };
  const focusOut = (e: FocusEvent<HTMLDivElement>) => {
    if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setFocus(false);
  };
  const label = pinned ? "Collapse contents" : "Pin contents open";
  return (
    <div
      className={cx(styles.rail, className)}
      data-toc-rail=""
      data-pinned={pinned ? "" : undefined}
      data-open={open ? "" : undefined}
      onPointerEnter={(e) => e.pointerType === "mouse" && setHover(true)}
      onPointerLeave={() => setHover(false)}
      onFocus={focusIn}
      onBlur={focusOut}
    >
      <div className={styles.head}>
        <span className={styles.label} aria-hidden="true">
          Contents
        </span>
        <button
          type="button"
          className={styles.pin}
          aria-pressed={pinned}
          aria-label={label}
          title={label}
          onClick={() => {
            setPinned(!pinned);
            setHover(false);
          }}
        >
          <svg aria-hidden="true" viewBox="0 0 16 16">
            <path d={pinned ? "M10 4L6 8l4 4" : "M3 4h10M3 8h7M3 12h10"} />
          </svg>
        </button>
      </div>
      <TocList {...props} follow />
    </div>
  );
}

// The list folded under "Contents" (narrow screens and panes).
export function TocFold({ className, ...props }: TocProps & { className?: string }) {
  if (!props.entries.length) return null;
  return (
    <details className={cx(styles.fold, className)}>
      <summary>Contents</summary>
      <TocList {...props} />
    </details>
  );
}

const NONE: TocHeading[] = [];

// The h1-h3 with ids inside `root`, read again when `key` changes.
export function useHeadings(root: { current: HTMLElement | null }, key: unknown, on = true) {
  const [headings, setHeadings] = useState<TocHeading[]>([]);
  useEffect(() => {
    const el = root.current;
    if (!on || !el) return;
    const read = () => {
      const sel = TOC_LEVELS.map((t) => `${t}[id]`).join(",");
      setHeadings(
        Array.from(el.querySelectorAll<HTMLElement>(sel)).map((e) => ({
          id: e.id,
          level: Number(e.tagName.slice(1)),
          text: e.textContent || "",
        })),
      );
    };
    read();
  }, [root, key, on]);
  return on ? headings : NONE;
}

// The entry the reader is in: the last whose heading has passed a line 20%
// down the scrolling box (the window when `box` is not given), the last at
// the box's end.
export function useScrollSpy(entries: TocEntry[], box?: { current: HTMLElement | null }) {
  const [active, setActive] = useState("");
  useEffect(() => {
    if (!entries.length) return;
    const el = box ? box.current : null;
    if (box && !el) return;
    const target: HTMLElement | Window = el || window;
    let frame = 0;
    const update = () => {
      frame = 0;
      const root = el || document.scrollingElement!;
      const top = el ? el.getBoundingClientRect().top : 0;
      const height = el ? el.clientHeight : innerHeight;
      const tops = entries.map(
        (h) => (document.getElementById(h.id)?.getBoundingClientRect().top ?? Infinity) - top,
      );
      const atEnd = root.scrollTop > 0 && root.scrollTop + height >= root.scrollHeight - 2;
      const i = activeHeading(tops, height * 0.2, atEnd);
      setActive(i < 0 ? "" : entries[i].id);
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    update();
    target.addEventListener("scroll", schedule, { passive: true });
    addEventListener("resize", schedule);
    return () => {
      target.removeEventListener("scroll", schedule);
      removeEventListener("resize", schedule);
      cancelAnimationFrame(frame);
    };
  }, [entries, box]);
  return entries.length ? active : "";
}
