"use client";
import { useEffect, useId, useRef, useState } from "react";
import { cx } from "./cx";
import styles from "./select.module.css";
export type Option = { value: string; label: string };
export const choices = (values: string[]) =>
  values.map((value) => ({ value, label: value }));
// Styled listbox select (ported from tn-eng-dashboard, owner request
// 20261103: "drop down is ugly, make it prettier"). A combobox button opens an
// opaque menu. Keys: ArrowDown/ArrowUp open on the next/previous option and
// move; Home/End; Enter/Space commit; Escape/Tab close; typing jumps by prefix.
// Focus stays on the trigger (aria-activedescendant). `data-value` exposes the
// committed value for tests and styling.
export function Select({
  label,
  ariaLabel,
  value,
  options,
  onChange,
  className,
}: {
  label?: string;
  ariaLabel?: string;
  value: string;
  options: Option[];
  onChange: (value: string) => void;
  className?: string;
}) {
  const id = useId();
  const root = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const prefix = useRef("");
  const lastKey = useRef(0);
  const current = Math.max(
    0,
    options.findIndex((o) => o.value === value),
  );
  useEffect(() => {
    if (!open) return;
    const dismiss = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", dismiss);
    return () => document.removeEventListener("pointerdown", dismiss);
  }, [open]);
  useEffect(() => {
    if (open)
      document
        .getElementById(`${id}-${active}`)
        ?.scrollIntoView({ block: "nearest" });
  }, [active, open, id]);
  const reveal = (at = current) => {
    setActive(Math.min(options.length - 1, Math.max(0, at)));
    setOpen(true);
  };
  const commit = (i: number) => {
    setOpen(false);
    if (options[i] && options[i].value !== value) onChange(options[i].value);
  };
  const selected = options[current];
  return (
    <div
      ref={root}
      data-select=""
      className={cx(styles.field, className)}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget)) setOpen(false);
      }}
    >
      {label && (
        <span id={`${id}-label`} data-caption="" className={styles.caption}>
          {label}
        </span>
      )}
      <button
        type="button"
        role="combobox"
        className={styles.trigger}
        aria-label={ariaLabel}
        aria-labelledby={ariaLabel ? undefined : `${id}-label`}
        aria-expanded={open}
        aria-controls={`${id}-list`}
        aria-haspopup="listbox"
        aria-activedescendant={open ? `${id}-${active}` : undefined}
        data-value={value}
        onClick={() => (open ? setOpen(false) : reveal())}
        onKeyDown={(e) => {
          if (e.key === "Tab") return setOpen(false);
          if (e.key === "Escape") {
            if (!open) return;
            e.preventDefault();
            e.stopPropagation();
            return setOpen(false);
          }
          const step = { ArrowDown: 1, ArrowUp: -1 }[e.key];
          if (step) {
            e.preventDefault();
            e.stopPropagation();
            return open
              ? setActive((active + step + options.length) % options.length)
              : reveal(current + step);
          }
          if (["Home", "End", "Enter", " "].includes(e.key)) {
            e.preventDefault();
            e.stopPropagation();
            if (!open) return reveal();
            if (e.key === "Home") return setActive(0);
            if (e.key === "End") return setActive(options.length - 1);
            return commit(active);
          }
          if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
            e.stopPropagation();
            prefix.current =
              Date.now() - lastKey.current > 700 ? e.key : prefix.current + e.key;
            lastKey.current = Date.now();
            const found = options.findIndex((o) =>
              o.label.toLowerCase().startsWith(prefix.current.toLowerCase()),
            );
            if (found >= 0) reveal(found);
          }
        }}
      >
        <span className={styles.value} title={selected?.label}>
          {selected?.label ?? value}
        </span>
        <svg aria-hidden="true" viewBox="0 0 16 16" className={styles.chevron}>
          <path d="M4 6l4 4 4-4" />
        </svg>
      </button>
      {open && (
        <div
          id={`${id}-list`}
          role="listbox"
          className={styles.menu}
          aria-label={ariaLabel}
          aria-labelledby={ariaLabel ? undefined : `${id}-label`}
        >
          {options.map((o, i) => (
            <div
              key={o.value}
              id={`${id}-${i}`}
              role="option"
              aria-selected={o.value === value}
              data-value={o.value}
              data-active={active === i}
              className={styles.option}
              onPointerMove={() => setActive(i)}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => commit(i)}
            >
              <span>{o.label}</span>
              <svg aria-hidden="true" viewBox="0 0 16 16" className={styles.check}>
                {o.value === value && <path d="M3 8.5l3 3 7-7" />}
              </svg>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
