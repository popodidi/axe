"use client";
// The one table of contents (the doc page, rendered run prompts): a compact
// numbered list of a body's headings with the section in view highlighted.
// `TocRail` is the sticky side list, `TocFold` the "Contents" disclosure for
// narrow space; the consumer's CSS shows one of them. The rail is collapsed
// to a dense strip of ticks, one per heading, that magnifies under the
// pointer like a dock (lib/toc-dock.ts) and names the tick it is on; keyboard
// focus opens the list over the content, and the toggle pins it open
// (lib/toc-pin.ts); a consumer widens its column for a pinned rail with
// `:has([data-pinned])`, or passes `fixed` for a rail of one width either
// way that slides between the strip and the list. Entries come from
// lib/doc-toc.ts (`tocEntries`); a short body has none, and both render
// nothing.
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type FocusEvent,
  type MouseEvent,
  type PointerEvent,
  type RefObject,
} from "react";
import { activeHeading, TOC_LEVELS, type TocEntry, type TocHeading } from "../../lib/doc-toc";
import { motionMs } from "../../lib/motion";
import { dockFocus, dockLayout } from "../../lib/toc-dock";
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

// The dock: while the pointer is on the collapsed rail's ticks, the tick
// under it grows and the rows around it open (lib/toc-dock.ts), following
// the pointer each frame; leaving eases them back. The magnification eases
// in and out over the feedback duration (none with reduced motion: the tick
// is only highlighted). The tick under the pointer is marked `data-dock` and
// its title shows in the rail's peek, a popover beside it. Each row's size
// and zoom are written as custom properties; the CSS uses them only when
// collapsed. The rows touch, so their tops follow in the flow.
function useDock(rail: RefObject<HTMLDivElement | null>, on: boolean) {
  // y: the pointer's clientY; u: the focus (lib/toc-dock.ts); h: the
  // magnification now, eased toward target; last: the previous frame's time
  // while easing.
  const dock = useRef({ y: 0, u: 0, h: 0, target: 0, frame: 0, last: 0, hovered: -1 });
  const clear = () => {
    const s = dock.current;
    cancelAnimationFrame(s.frame);
    Object.assign(s, { h: 0, target: 0, frame: 0, last: 0, hovered: -1 });
    const ol = rail.current?.querySelector("ol");
    ol?.querySelectorAll("li").forEach((li) => {
      for (const p of ["--dock-size", "--dock-zoom"]) li.style.removeProperty(p);
    });
    ol?.querySelector("[data-dock]")?.removeAttribute("data-dock");
    const label = peekOf(rail.current);
    if (label?.matches(":popover-open")) label.hidePopover();
  };
  const step = (now: number) => {
    const s = dock.current;
    s.frame = 0;
    const ol = rail.current?.querySelector("ol");
    if (!ol) return;
    const pitch = parseFloat(getComputedStyle(ol).getPropertyValue("--toc-pitch")) || 6;
    const tau = motionMs("--duration-feedback") / 3;
    const dt = s.last ? now - s.last : 16;
    s.last = now;
    s.h = tau ? s.h + (s.target - s.h) * (1 - Math.exp(-dt / tau)) : 0;
    if (Math.abs(s.target - s.h) < 0.005) s.h = tau ? s.target : 0;
    const items = ol.querySelectorAll("li");
    const o = ol.getBoundingClientRect();
    if (s.target) s.u = dockFocus((s.y - o.top) / pitch, s.h);
    const { rows } = dockLayout(items.length, s.u, s.h);
    if (!s.target && !s.h) return clear();
    items.forEach((li, i) => {
      li.style.setProperty("--dock-size", `${rows[i].size * pitch}px`);
      li.style.setProperty("--dock-zoom", rows[i].zoom.toFixed(3));
    });
    const hovered = s.target ? Math.min(items.length - 1, Math.max(0, Math.floor(s.u))) : -1;
    if (hovered !== s.hovered) {
      ol.querySelector("[data-dock]")?.removeAttribute("data-dock");
      items[hovered]?.querySelector("a")?.setAttribute("data-dock", "");
      s.hovered = hovered;
    }
    const label = peekOf(rail.current);
    if (label && hovered >= 0) {
      const a = items[hovered].querySelector("a")!;
      label.textContent = [styles.number, styles.text]
        .map((c) => a.querySelector(`.${c}`)?.textContent)
        .filter(Boolean)
        .join(" ");
      label.style.left = `${o.right}px`;
      label.style.top = `${o.top + (rows[hovered].top + rows[hovered].size / 2) * pitch}px`;
      if (!label.matches(":popover-open")) label.showPopover();
    } else if (label?.matches(":popover-open")) label.hidePopover();
    if (s.h !== s.target) s.frame = requestAnimationFrame(step);
    else s.last = 0;
  };
  const schedule = () => {
    if (!dock.current.frame) dock.current.frame = requestAnimationFrame(step);
  };
  const move = (e: PointerEvent<HTMLDivElement>) => {
    const ol = rail.current?.querySelector("ol");
    if (!on || e.pointerType !== "mouse" || !ol) return;
    const o = ol.getBoundingClientRect();
    const s = dock.current;
    s.y = e.clientY;
    // On the ticks only: a fixed rail is wider than its strip.
    s.target = s.y >= o.top && s.y < o.bottom && e.clientX < o.right ? 1 : 0;
    schedule();
  };
  const leave = () => {
    dock.current.target = 0;
    schedule();
  };
  // Off (pinned, or the list open) or gone: the rows return to rest at once.
  // On, a scroll under a still pointer moves the focus too.
  useEffect(() => {
    if (!on) return clear();
    const scroll = () => dock.current.target && schedule();
    document.addEventListener("scroll", scroll, { capture: true, passive: true });
    return () => {
      document.removeEventListener("scroll", scroll, { capture: true });
      clear();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [on]);
  return { move, leave };
}

// The rail's popover naming the hovered tick.
const peekOf = (rail: HTMLElement | null) => rail?.querySelector<HTMLElement>(`.${styles.peek}`);

// The sticky side list. Collapsed it is a dense strip of ticks that
// magnifies under the pointer (useDock); keyboard focus opens the full list
// over the content (Escape closes it), and the toggle keeps it open in the
// page's flow. A `fixed` rail keeps one width (its column's) collapsed or
// open; the toggle slides the list in from the strip and, unpinning, out
// again before the strip slides back (`leaving`: the choice is stored at
// once, the pinned list stays shown for the slide; a second press reverses
// it). Reduced motion swaps them at once.
export function TocRail({ className, fixed = false, ...props }: TocProps & { className?: string; fixed?: boolean }) {
  const [pinned, setPinned] = useTocPinned();
  const [focus, setFocus] = useState(false);
  // Set by the toggle (and, on a fixed rail, by keyboard focus): a pin the
  // reader just made fades in; one restored on load does not.
  const [reveal, setReveal] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const leave = useRef(0);
  useEffect(() => () => clearTimeout(leave.current), []);
  const rail = useRef<HTMLDivElement>(null);
  const shown = pinned || leaving;
  const open = !shown && focus && props.entries.length > 0;
  const dock = useDock(rail, !shown && !open);
  // Escape closes the open list wherever focus is; focus stays where it was.
  useEffect(() => {
    if (!open) return;
    const key = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      setFocus(false);
    };
    document.addEventListener("keydown", key, true);
    return () => document.removeEventListener("keydown", key, true);
  }, [open]);
  if (!props.entries.length) return null;
  // Keyboard focus only: a clicked entry keeps focus, and the strip should
  // stay a strip.
  const focusIn = (e: FocusEvent<HTMLDivElement>) => {
    if (!e.target.matches(":focus-visible")) return;
    setFocus(true);
    if (fixed) setReveal(true);
  };
  const focusOut = (e: FocusEvent<HTMLDivElement>) => {
    if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setFocus(false);
  };
  const label = pinned ? "Collapse contents" : "Pin contents open";
  return (
    <div
      ref={rail}
      className={cx(styles.rail, className)}
      data-toc-rail=""
      data-fixed={fixed ? "" : undefined}
      data-pinned={shown ? "" : undefined}
      data-leaving={leaving ? "" : undefined}
      data-open={open ? "" : undefined}
      data-reveal={reveal ? "" : undefined}
      onPointerMove={dock.move}
      onPointerLeave={dock.leave}
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
            clearTimeout(leave.current);
            setPinned(!pinned);
            setFocus(false);
            setReveal(true);
            const ms = fixed && pinned ? motionMs("--duration-layout") : 0;
            setLeaving(ms > 0);
            if (ms) leave.current = window.setTimeout(() => setLeaving(false), ms);
          }}
        >
          {/* A side panel, its strip filled while pinned open: a glyph
              unlike the ticks below it. */}
          <svg aria-hidden="true" viewBox="0 0 16 16">
            <rect x="1.5" y="2.5" width="13" height="11" rx="2" />
            <path d="M6 2.5v11" />
            {pinned && <rect x="1.5" y="2.5" width="4.5" height="11" rx="1" fill="currentColor" />}
          </svg>
        </button>
      </div>
      <TocList {...props} follow />
      <span className={styles.peek} popover="manual" aria-hidden="true" />
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
