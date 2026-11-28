"use client";
// A doc's history on the doc page (doc-page.tsx): the versions list in the
// metadata rail, a notice over an older version, and the changes between
// two versions in place of the body. Each view is a URL (`version=`, or
// `diff=1` with optional `from=` and `to=`), so back and forward work and a
// view can be shared. Reads `/api/docs/log`, `/api/docs/file?version=` and
// `/api/docs/diff` (crates/axe/src/server.rs `doc_route`).
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { diffPair, hunkLabel, parseDiff } from "../../lib/doc-diff";
import { getJson } from "../../lib/store";
import { Alert, Button, Select, Skeleton, cx } from "../ui";
import { fullDate, type DocVersion } from "./store-viewer";
import styles from "./doc-history.module.css";

export type DocView = { version?: string; diff?: { from?: string; to?: string } };
export type DocHref = (view: DocView) => string;

const short = (v: string) => v.slice(0, 7);
// The rail lists this many versions until "Show all".
const FIRST = 5;

// Versions newest first: when, who and why, each opening that version and
// its changes. The shown version is marked.
export function DocHistory({
  versions,
  shown,
  diffTo,
  href,
}: {
  versions: DocVersion[];
  shown: string | null;
  diffTo: string | null;
  href: DocHref;
}) {
  const [all, setAll] = useState(false);
  const list = all ? versions : versions.slice(0, FIRST);
  return (
    <div className={styles.history}>
      <ol className={styles.versions} aria-label="Doc history">
        {list.map((v, i) => {
          const open = diffTo === null && shown === v.version;
          return (
            <li key={v.version} className={cx(styles.entry, open && styles.open)}>
              <span className={styles.when}>
                <Link
                  href={i === 0 ? href({}) : href({ version: v.version })}
                  aria-current={open ? "page" : undefined}
                  title={v.version}
                >
                  {fullDate(v.at)}
                </Link>
                {i === 0 && <span className={styles.tag}>current</span>}
              </span>
              <span className={styles.who}>
                {v.author || "—"} · <span className={styles.mono}>{short(v.version)}</span>
              </span>
              {v.message && <span className={styles.why}>{v.message}</span>}
              <Link
                className={styles.changes}
                href={href({ diff: { to: v.version } })}
                aria-current={diffTo === v.version ? "page" : undefined}
                aria-label={`Changes in the version of ${fullDate(v.at)}`}
              >
                {i === versions.length - 1 ? "Created" : "Changes"}
              </Link>
            </li>
          );
        })}
      </ol>
      {versions.length > FIRST && (
        <Button variant="link" onClick={() => setAll(!all)}>
          {all ? "Show fewer" : `Show all ${versions.length} versions`}
        </Button>
      )}
    </div>
  );
}

// Over an older version: it is read only, with the way back.
export function OldVersionNotice({
  version,
  href,
}: {
  version: DocVersion | { version: string; at?: undefined; author?: undefined };
  href: DocHref;
}) {
  return (
    <div className={styles.notice} role="note" aria-label="Older version">
      <span>
        An older version
        {version.at ? `, ${fullDate(version.at)}` : ""}
        {version.author ? ` by ${version.author}` : ""}. Read only.
      </span>
      <span className={styles.noticeLinks}>
        <Link href={href({})}>Current version</Link>
        <Link href={href({ diff: { to: version.version } })}>Changes in this version</Link>
      </span>
    </div>
  );
}

type Loaded = { key: string; diff?: string; error?: string };

// The changes between two versions (default: the one before the current
// version, and the current one), each file's added and removed lines
// tinted and marked, long lines wrapped to the column.
export function DocDiffView({
  base,
  path,
  folder,
  versions,
  from,
  to,
  href,
}: {
  base: string;
  path: string;
  folder: boolean;
  versions: DocVersion[] | null;
  from?: string;
  to?: string;
  href: DocHref;
}) {
  const router = useRouter();
  const pair = versions && diffPair(versions.map((v) => v.version), from, to);
  const key = pair ? `${pair.from}..${pair.to}` : "";
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  useEffect(() => {
    if (!pair) return;
    let live = true;
    const q = new URLSearchParams({ path });
    if (pair.from) q.set("from", pair.from);
    if (pair.to) q.set("to", pair.to);
    getJson<{ diff: string }>(`${base}/api/docs/diff?${q}`)
      .then((d) => live && setLoaded({ key, diff: d.diff }))
      .catch((e) => live && setLoaded({ key, error: (e as Error).message }));
    return () => {
      live = false;
    };
    // `key` names the pair.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [base, path, key]);

  const label = (v: DocVersion) =>
    `${fullDate(v.at)} · ${v.author ? `${v.author} · ` : ""}${short(v.version)}`;
  const options = (versions || []).map((v) => ({ value: v.version, label: label(v) }));
  const go = (f: string | null, t: string) =>
    router.push(href({ diff: { from: f || undefined, to: t } }));
  const current = loaded && loaded.key === key ? loaded : null;
  const files = current?.diff !== undefined ? parseDiff(current.diff, path) : null;
  const added = files?.reduce((n, f) => n + f.added, 0) ?? 0;
  const removed = files?.reduce((n, f) => n + f.removed, 0) ?? 0;
  return (
    <section className={styles.diffView} aria-label="Changes">
      <header className={styles.diffHead}>
        <h2 className={styles.diffTitle}>Changes</h2>
        {files && (
          <span className={styles.counts} aria-label={`${added} added, ${removed} removed`}>
            <span className={styles.plus}>+{added}</span>{" "}
            <span className={styles.minus}>−{removed}</span>
          </span>
        )}
        <Link className={styles.back} href={href({})}>
          Back to the current version
        </Link>
      </header>
      {pair && options.length > 0 && (
        <div className={styles.pickers}>
          <Select
            label="From"
            value={pair.from || ""}
            options={
              pair.from ? options : [{ value: "", label: "Nothing (new doc)" }, ...options]
            }
            onChange={(f) => go(f, pair.to)}
          />
          <Select label="To" value={pair.to} options={options} onChange={(t) => go(pair.from, t)} />
        </div>
      )}
      {!current ? (
        <Skeleton label="Comparing…" lines={4} />
      ) : current.error ? (
        <Alert>Unable to compare these versions: {current.error}</Alert>
      ) : files && files.length === 0 ? (
        <p className={styles.same}>No changes between these versions.</p>
      ) : (
        files?.map((f) => (
          <div key={f.path} className={styles.diffFile}>
            {folder && (
              <h3 className={styles.diffFileName}>
                {f.path}{" "}
                <span className={styles.counts}>
                  <span className={styles.plus}>+{f.added}</span>{" "}
                  <span className={styles.minus}>−{f.removed}</span>
                </span>
              </h3>
            )}
            <div className={styles.lines} data-diff="">
              {f.lines.map((l, i) =>
                l.kind === "hunk" ? (
                  <div key={i} className={styles.hunk} title={l.text}>
                    {hunkLabel(l.text)}
                  </div>
                ) : l.kind === "add" ? (
                  <ins key={i} className={cx(styles.line, styles.add)}>
                    <span className={styles.mark} aria-hidden>
                      +
                    </span>
                    {l.text || " "}
                  </ins>
                ) : l.kind === "remove" ? (
                  <del key={i} className={cx(styles.line, styles.remove)}>
                    <span className={styles.mark} aria-hidden>
                      −
                    </span>
                    {l.text || " "}
                  </del>
                ) : (
                  <div key={i} className={cx(styles.line, l.kind === "note" && styles.note)}>
                    <span className={styles.mark} aria-hidden>
                      {" "}
                    </span>
                    {l.text || " "}
                  </div>
                ),
              )}
            </div>
          </div>
        ))
      )}
    </section>
  );
}
