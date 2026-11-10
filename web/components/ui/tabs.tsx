"use client";
import { useLayoutEffect, useRef, type ReactNode } from "react";
import { cx } from "./cx";
import styles from "./tabs.module.css";
// One indicator moves to the active tab. It is placed without a transition on
// mount (and for restored URLs); later changes travel 160ms (zero with reduced
// motion). Focus stays on the activated tab.
export function Tabs({
  tabs,
  value,
  onChange,
  className,
}: {
  tabs: string[];
  value: string;
  onChange: (tab: string) => void;
  className?: string;
}) {
  const list = useRef<HTMLDivElement>(null);
  const indicator = useRef<HTMLSpanElement>(null);
  useLayoutEffect(() => {
    const root = list.current;
    const bar = indicator.current;
    if (!root || !bar) return;
    const place = () => {
      const tab = root.querySelector<HTMLElement>('[aria-selected="true"]');
      bar.style.setProperty("--tab-left", (tab?.offsetLeft ?? 0) + "px");
      bar.style.setProperty("--tab-width", (tab?.offsetWidth ?? 0) + "px");
    };
    place();
    const frame = requestAnimationFrame(() => (bar.dataset.ready = "true"));
    const observer = new ResizeObserver(place);
    observer.observe(root);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [value]);
  return (
    <div ref={list} role="tablist" className={cx(styles.tabs, className)}>
      {tabs.map((t) => (
        <Tab key={t} selected={value === t} onClick={() => onChange(t)}>
          {t}
        </Tab>
      ))}
      <span ref={indicator} aria-hidden className={styles.indicator} />
    </div>
  );
}
function Tab({
  selected,
  onClick,
  children,
}: {
  selected: boolean;
  onClick: () => void;
  children: string;
}) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={selected}
      data-label={children}
      className={styles.tab}
      onClick={onClick}
    >
      {children as ReactNode}
    </button>
  );
}
