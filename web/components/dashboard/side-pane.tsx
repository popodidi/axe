"use client";
// The one list/detail grammar every list view shares (Tasks, Routines,
// Services, Bots, Goals, Store): a selected row opens its item in a right side
// pane with the same header (kind, state badge, Close/Esc, title), a field
// grid, titled sections, and one empty-state wording for lists.
import type { ComponentProps, ReactNode, Ref } from "react";
import { Button, EmptyState, Kbd, cx } from "../ui";
import styles from "./side-pane.module.css";

export type Tone = "ok" | "warn" | "bad" | "active" | "pending" | "neutral";

// A state label: text always, colour only as a second cue.
export function StateBadge({ tone, children }: { tone: Tone; children: ReactNode }) {
  return (
    <span className={styles.badge} data-tone={tone}>
      {children}
    </span>
  );
}

// Pane header: the item's kind or id, its state, Close (Esc), then the title.
// The title is a programmatically focusable heading for keyboard opening.
export function PaneHeader({
  kind,
  badge,
  title,
  onClose,
  closeLabel = "Close",
  headingRef,
}: {
  kind: ReactNode;
  badge?: ReactNode;
  title?: ReactNode;
  onClose: () => void;
  closeLabel?: string;
  headingRef?: Ref<HTMLHeadingElement>;
}) {
  return (
    <header className={styles.header}>
      <div className={styles.top}>
        {typeof kind === "string" ? <span className={styles.kind}>{kind}</span> : kind}
        {badge}
        <Button variant="ghost" className={styles.close} onClick={onClose}>
          {closeLabel} <Kbd>Esc</Kbd>
        </Button>
      </div>
      {title !== undefined && (
        <h2 ref={headingRef} tabIndex={-1} className={styles.title}>
          {title}
        </h2>
      )}
    </header>
  );
}

// A side pane beside the list (Routines, Services, Bots, Goals); a task pane
// may stack to its right. Tasks and the Store use the dashboard's animated
// detail pane with the same PaneHeader.
export function ItemPane({
  id,
  label,
  kind,
  badge,
  title,
  onClose,
  children,
}: {
  id: string;
  label: string;
  kind: ReactNode;
  badge?: ReactNode;
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
}) {
  return (
    <section id={id} data-item-pane="" className={styles.pane} aria-label={label}>
      <PaneHeader kind={kind} badge={badge} title={title} onClose={onClose} />
      <div className={styles.body}>{children}</div>
    </section>
  );
}

export type Fact = {
  label: ReactNode;
  value: ReactNode;
  /** Spans the whole grid row (commands, paths, long text). */
  wide?: boolean;
  mono?: boolean;
};

// Key facts in one inset grid, label above value. `children` adds facts that
// render their own <div><dt/><dd/></div> (they may load lazily).
export function Facts({
  items,
  children,
}: {
  items: (Fact | false | null | undefined)[];
  children?: ReactNode;
}) {
  return (
    <dl className={styles.facts}>
      {items.filter((f): f is Fact => !!f).map((f, i) => (
        <div key={i} className={f.wide ? styles.wide : undefined}>
          <dt>{f.label}</dt>
          <dd className={f.mono ? styles.mono : undefined}>{f.value}</dd>
        </div>
      ))}
      {children}
    </dl>
  );
}

// A titled pane section (logs, history, intent…): a region named by its title.
export function PaneSection({
  title,
  note,
  label,
  className,
  children,
  ...props
}: {
  title: ReactNode;
  note?: ReactNode;
  label?: string;
} & ComponentProps<"section">) {
  return (
    <section
      className={cx(styles.section, className)}
      aria-label={label ?? (typeof title === "string" ? title : undefined)}
      {...props}
    >
      <h3 className={styles.sectionTitle}>{title}</h3>
      {note && <p className={styles.note}>{note}</p>}
      {children}
    </section>
  );
}

// A list's empty state: nothing at all, or nothing matching the filters (with
// a way back).
export function ListEmpty({
  total,
  noun,
  empty,
  onClear,
}: {
  total: number;
  noun: string;
  empty: ReactNode;
  onClear?: () => void;
}) {
  return (
    <EmptyState>
      {total ? (
        <>
          No matching {noun}.
          {onClear && (
            <>
              {" "}
              <Button variant="link" onClick={onClear}>
                Clear filters
              </Button>
            </>
          )}
        </>
      ) : (
        empty
      )}
    </EmptyState>
  );
}
