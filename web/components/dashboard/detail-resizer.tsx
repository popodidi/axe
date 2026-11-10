"use client";
import {
  useCallback,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent,
} from "react";
import { setPaneWidth } from "../../lib/layout";
import type { Pane } from "../../lib/layout-script";
import styles from "./detail-resizer.module.css";
// Arrow keys move the divider 16px (64px with Shift); Home/End jump to the
// narrowest/widest pane. Double-click restores the default width. Dragging
// follows the pointer with no easing.
const step = 16;
const bigStep = 64;
type Bounds = { now: number; min: number; max: number };
const px = (v: string) => (/^[\d.]+px$/.test(v) ? parseFloat(v) : NaN);
// A separator beside the pane it sizes: `side="next"` sizes the following
// sibling (the separator sits on its left edge), `side="prev"` the preceding
// one (on its right edge). CSS clamps the pane too, so viewport changes never
// push it off-screen.
export function PaneResizer({
  controls,
  label,
  pane,
  side = "next",
}: {
  controls: string;
  label: string;
  pane: Pane;
  side?: "next" | "prev";
}) {
  const ref = useRef<HTMLDivElement>(null);
  const drag = useRef<{ x: number; width: number } | null>(null);
  const [bounds, setBounds] = useState<Bounds>({ now: 0, min: 0, max: 0 });
  const target = useCallback(() => {
    const sep = ref.current;
    return (
      side === "next" ? sep?.nextElementSibling : sep?.previousElementSibling
    ) as HTMLElement | null | undefined;
  }, [side]);
  const measure = useCallback((): Bounds | null => {
    const sep = ref.current;
    const el = target();
    const parent = sep?.parentElement;
    if (!sep || !el || !parent) return null;
    const cs = getComputedStyle(el);
    const min = px(cs.minWidth) || 0;
    // Room left by the other panes: flexible ones keep their minimum, fixed
    // ones their current width.
    let others = 0;
    for (const child of Array.from(parent.children) as HTMLElement[]) {
      if (child === el || child.getAttribute("role") === "separator") continue;
      if (!child.getClientRects().length) continue;
      const c = getComputedStyle(child);
      others +=
        parseFloat(c.flexGrow) > 0
          ? px(c.minWidth) || 0
          : child.getBoundingClientRect().width;
    }
    const cssMax = px(cs.maxWidth);
    const room = parent.clientWidth - others;
    const max = Math.max(min, Number.isFinite(cssMax) ? Math.min(cssMax, room) : room);
    const next = {
      now: Math.round(el.getBoundingClientRect().width),
      min: Math.round(min),
      max: Math.floor(max),
    };
    setBounds(next);
    return next;
  }, [target]);
  useLayoutEffect(() => {
    measure();
    const parent = ref.current?.parentElement;
    const el = target();
    if (!parent || !el) return;
    const observer = new ResizeObserver(() => measure());
    observer.observe(parent);
    observer.observe(el);
    return () => observer.disconnect();
  }, [measure, target]);
  const resize = useCallback(
    (width: number, persist = true) => {
      const b = measure();
      if (!b) return;
      setPaneWidth(pane, Math.round(Math.min(b.max, Math.max(b.min, width))), persist);
      measure();
    },
    [measure, pane],
  );
  const sign = side === "next" ? -1 : 1;
  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    e.currentTarget.focus();
    drag.current = { x: e.clientX, width: measure()?.now ?? 0 };
    e.currentTarget.dataset.dragging = "true";
    // Width transitions (the nav rail) are bypassed while dragging.
    document.documentElement.dataset.resizing = "";
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    resize(drag.current.width + sign * (e.clientX - drag.current.x), false);
  };
  const endDrag = (e: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    drag.current = null;
    delete e.currentTarget.dataset.dragging;
    delete document.documentElement.dataset.resizing;
    const b = measure();
    if (b) setPaneWidth(pane, b.now);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const b = measure();
    if (!b) return;
    const by = e.shiftKey ? bigStep : step;
    const target = {
      ArrowLeft: b.now - sign * by,
      ArrowRight: b.now + sign * by,
      Home: b.min,
      End: b.max,
    }[e.key];
    if (target === undefined) return;
    e.preventDefault();
    resize(target);
  };
  return (
    <div
      ref={ref}
      role="separator"
      tabIndex={0}
      aria-orientation="vertical"
      aria-label={label}
      aria-controls={controls}
      aria-valuenow={bounds.now}
      aria-valuemin={bounds.min}
      aria-valuemax={bounds.max}
      aria-valuetext={`${bounds.now} pixels wide`}
      title="Drag or use arrow keys to resize; double-click to reset"
      className={styles.resizer}
      data-side={side}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      onKeyDown={onKeyDown}
      onDoubleClick={() => {
        setPaneWidth(pane, null);
        measure();
      }}
    />
  );
}
export const DetailResizer = ({ controls }: { controls: string }) => (
  <PaneResizer controls={controls} label="Resize detail pane" pane="detail" />
);
