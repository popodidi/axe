import { Fragment, useId, useState } from "react";
import type { Event } from "../../lib/model";
import { artifactReference } from "../../lib/artifact-reference";
import { Button, CopyButton, EmptyState, Filter, Search } from "../ui";
import type { CopyHandler } from "../ui/command";
import styles from "./artifact-table.module.css";
export function ArtifactTable({
  events,
  onCopy,
}: {
  events: Event[];
  onCopy: CopyHandler;
}) {
  const detailsId = useId();
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  const [query, setQuery] = useState("");
  const [type, setType] = useState("");
  const rows = events
    .map((event) => {
      const value = event.body || event.to_value || "";
      return { event, value, ...artifactReference(value) };
    })
    .sort(
      (a, b) =>
        (Date.parse(b.event.at) || 0) - (Date.parse(a.event.at) || 0) ||
        b.event.id - a.event.id,
    );
  const filtered = rows.filter(
    (r) =>
      (!type || r.type === type) &&
      `${r.value} ${r.name} ${r.source} ${r.type}`
        .toLowerCase()
        .includes(query.toLowerCase()),
  );
  if (!events.length)
    return <EmptyState>No artifacts recorded for this task.</EmptyState>;
  return (
    <section aria-label="Artifacts" className={styles.section}>
      <div className={styles.toolbar}>
        <span role="status">
          {filtered.length} of {rows.length} artifacts · Newest first
        </span>
        <Search
          aria-label="Search artifacts"
          placeholder="Search references"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <Filter
          label="Type"
          aria-label="Artifact type"
          value={type}
          onChange={setType}
          options={[
            { value: "", label: "All types" },
            ...Array.from(new Set(rows.map((r) => r.type)))
              .sort()
              .map((value) => ({ value, label: value })),
          ]}
        />
      </div>
      {filtered.length ? (
        <table className={styles.table} aria-label="Artifact references">
          <thead>
            <tr>
              <th>Type</th>
              <th>Name or reference</th>
              <th className={styles.source}>Source</th>
              <th>Added</th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((r) => (
              <Fragment key={r.event.id}>
                <tr data-artifact>
                  <td>{r.type}</td>
                  <td>
                    <div className={styles.name}>
                      {r.href ? (
                        <a
                          href={r.href}
                          target="_blank"
                          rel="noopener noreferrer"
                          title={r.value}
                        >
                          {r.name}
                        </a>
                      ) : (
                        <span title={r.value}>{r.name}</span>
                      )}
                    </div>
                    <Button
                      className={styles.disclosure}
                      variant="ghost"
                      aria-label={`Reference details: ${r.name}`}
                      aria-expanded={expanded.has(r.event.id)}
                      aria-controls={`${detailsId}-${r.event.id}`}
                      onClick={() =>
                        setExpanded((previous) => {
                          const next = new Set(previous);
                          if (next.has(r.event.id)) next.delete(r.event.id);
                          else next.add(r.event.id);
                          return next;
                        })
                      }
                    >
                      {expanded.has(r.event.id) ? "Close" : "Details"}
                    </Button>
                  </td>
                  <td className={styles.source} title={r.source}>
                    {r.source}
                  </td>
                  <td>
                    <time dateTime={r.event.at || undefined} title={r.event.at}>
                      {Number.isFinite(Date.parse(r.event.at))
                        ? new Date(r.event.at).toLocaleDateString(undefined, {
                            month: "short",
                            day: "numeric",
                          })
                        : "—"}
                    </time>
                  </td>
                </tr>
                {expanded.has(r.event.id) && (
                  <tr>
                    <td colSpan={3}>
                      <div
                        id={`${detailsId}-${r.event.id}`}
                        data-reference-details
                        className={styles.full}
                      >
                        <span>{r.value || "Empty reference"}</span>
                        <CopyButton
                          text={r.value}
                          label={`Copy reference: ${r.name}`}
                          onCopy={onCopy}
                        />
                        <span>
                          Source: {r.source} · Added by{" "}
                          {r.event.actor || "Unknown"} ·{" "}
                          {r.event.at || "Unknown date"}
                        </span>
                      </div>
                    </td>
                    <td className={styles.source} aria-hidden="true" />
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      ) : (
        <EmptyState>No matching artifacts.</EmptyState>
      )}
    </section>
  );
}
