"use client";
// A doc's full page (app/doc): a table of contents for markdown in a sticky
// left rail, its metadata in a sticky right rail, the body in a wide center
// column. Narrower windows fold the contents into a "Contents" disclosure
// first, then stack the metadata above the body. The page scrolls with the
// window; a rail scrolls itself only when it is taller than the window.
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import Markdown from "../../app/markdown";
import { activeHeading, TOC_LEVELS } from "../../lib/doc-toc";
import { docKind } from "../../lib/doc-render";
import {
  docScopes,
  formatBytes,
  getJson,
  splitFrontmatter,
  useStoreSession,
  type DocAttachment,
  type StoreDoc,
} from "../../lib/store";
import { Alert, cx, Skeleton } from "../ui";
import { DocFile } from "./doc-file";
import { Facts, fullDate, StoreUnlock, type DocBody, type DocVersion } from "./store-viewer";
import styles from "./doc-page.module.css";

// Only markdown (lib/doc-render.ts) gets the contents rail and frontmatter
// in the metadata rail.
const isMarkdown = (path: string, content: string, contentType?: string) =>
  docKind(path, content, contentType) === "markdown";

type Heading = { id: string; level: number; text: string };

// The store's listing row (null when the server lists none) and the scopes
// the doc is attached under.
type StoreFacts = { row: StoreDoc | null; scopes: string[] };

export function DocPage({ base, path }: { base: string; path: string }) {
  const session = useStoreSession(base);
  const [doc, setDoc] = useState<DocBody | null>(null);
  const [versions, setVersions] = useState<DocVersion[] | null>(null);
  const [store, setStore] = useState<StoreFacts | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!session.read) return;
    let live = true;
    const q = encodeURIComponent(path);
    getJson<DocBody>(`${base}/api/docs/file?path=${q}`)
      .then((d) => live && setDoc(d))
      .catch((e) => live && setError((e as Error).message));
    getJson<DocVersion[]>(`${base}/api/docs/log?path=${q}`)
      .then((v) => live && setVersions(v))
      .catch(() => live && setVersions([]));
    Promise.all([
      getJson<StoreDoc[]>(`${base}/api/docs?prefix=${q}`).catch(() => []),
      getJson<DocAttachment[]>(`${base}/api/docs/attachments`).catch(() => []),
    ]).then(([rows, attachments]) => {
      if (live)
        setStore({
          row: rows.find((r) => r.path === path) || null,
          scopes: docScopes(path, attachments),
        });
    });
    return () => {
      live = false;
    };
  }, [base, path, session.read]);

  if (session.read === false)
    return <StoreUnlock base={base} reason="Unlock the store to read docs." />;
  if (error) return <Alert>Unable to open this doc: {error}</Alert>;
  if (!doc) return <Skeleton label="Opening…" lines={3} />;
  const single = doc.kind !== "folder";
  const files = single ? [{ path: "", content: doc.content || "" }] : doc.files || [];
  // A single markdown doc's frontmatter moves to the metadata rail.
  const split =
    single && isMarkdown(doc.path, files[0].content, doc.content_type)
      ? splitFrontmatter(files[0].content)
      : null;
  const markdown = files.some((f) =>
    isMarkdown(f.path || doc.path, f.content, single ? doc.content_type : undefined),
  );
  // The rail's header: the file name, its folder above it.
  const cut = doc.path.lastIndexOf("/") + 1;
  const [dir, name] = [doc.path.slice(0, cut), doc.path.slice(cut)];
  return (
    <DocLayout
      markdown={markdown}
      bodyKey={doc.version}
      meta={
        <>
          <header className={styles.head}>
            <span className={styles.path} title={doc.path}>
              {dir && <span className={styles.dir}>{dir}</span>}
              <span className={styles.name}>{name}</span>
            </span>
            <span className={styles.version}>
              {doc.kind} · version <span className={styles.mono}>{doc.version.slice(0, 12)}</span>
            </span>
          </header>
          <RailFacts
            title="Details"
            label="Doc details"
            rows={[
              ["Version", <Clipped key="v" text={doc.version} mono />],
              [
                "Versions",
                versions === null
                  ? "…"
                  : versions.length
                    ? `${versions.length} · first ${fullDate(versions[versions.length - 1].at)}`
                    : "—",
              ],
              ["Size", store === null ? "…" : store.row ? formatBytes(store.row.size) : "—"],
              ["Updated", store === null ? "…" : fullDate(store.row?.updated_at)],
              [
                "Scopes",
                store === null
                  ? "…"
                  : store.scopes.length
                    ? store.scopes.map((s) => <Clipped key={s} text={s} />)
                    : "None",
              ],
            ]}
          />
          {split && split.meta.length > 0 && (
            <RailFacts
              title="Frontmatter"
              label="Frontmatter"
              rows={split.meta.map(([k, v]) => [
                k,
                /\s/.test(v) ? v : <Clipped key={k} text={v} />,
              ])}
            />
          )}
        </>
      }
    >
      {files.map((f, i) => (
        <section key={f.path} aria-label={f.path || doc.path} className={styles.file}>
          {f.path && <h2 className={styles.fileName}>{f.path}</h2>}
          <DocFile
            path={f.path || doc.path}
            content={f.content}
            contentType={single ? doc.content_type : undefined}
            markdown={(text) => (
              <MarkdownFile
                text={split ? split.body : text}
                frontmatter={!split}
                prefix={single ? "" : `f${i}-`}
              />
            )}
          />
        </section>
      ))}
    </DocLayout>
  );
}

// A folder file keeps its frontmatter above its own body.
function MarkdownFile({
  text,
  frontmatter,
  prefix,
}: {
  text: string;
  frontmatter: boolean;
  prefix: string;
}) {
  const { meta, body } = frontmatter ? splitFrontmatter(text) : { meta: [], body: text };
  return (
    <>
      {meta.length > 0 && <Facts label="Frontmatter" rows={meta} />}
      <Markdown body={body} headingIds={prefix} document />
    </>
  );
}

// A labelled key/value group in the metadata rail: muted keys in one column,
// values aligned beside them.
function RailFacts({
  title,
  label,
  rows,
}: {
  title: string;
  label: string;
  rows: [string, ReactNode][];
}) {
  return (
    <section className={styles.group}>
      <h2 className={styles.railLabel}>{title}</h2>
      <dl className={styles.facts} aria-label={label}>
        {rows.map(([k, v]) => (
          <div key={k} className={styles.fact}>
            <dt>{k}</dt>
            <dd>{v}</dd>
          </div>
        ))}
      </dl>
    </section>
  );
}

// One line of a long unbroken value (a hash, a scope, a URL): cut with an
// ellipsis, whole in its tooltip and when selected and copied.
function Clipped({ text, mono = false }: { text: string; mono?: boolean }) {
  return (
    <span className={cx(styles.clip, mono && styles.mono)} title={text}>
      {text}
    </span>
  );
}

function DocLayout({
  meta,
  markdown,
  bodyKey,
  children,
}: {
  meta: ReactNode;
  markdown: boolean;
  bodyKey: string;
  children: ReactNode;
}) {
  const body = useRef<HTMLElement>(null);
  const [headings, setHeadings] = useState<Heading[]>([]);
  const [active, setActive] = useState("");
  // The rendered body is the source of the contents: its h1-h3 with ids.
  useEffect(() => {
    if (!markdown || !body.current) return;
    const els = Array.from(
      body.current.querySelectorAll<HTMLElement>(TOC_LEVELS.map((t) => `${t}[id]`).join(",")),
    );
    setHeadings(
      els.map((e) => ({
        id: e.id,
        level: Number(e.tagName.slice(1)),
        text: e.textContent || "",
      })),
    );
    // The browser looked for the anchor before the doc loaded.
    const id = decodeURIComponent(location.hash.slice(1));
    if (id) document.getElementById(id)?.scrollIntoView();
  }, [markdown, bodyKey]);
  // The current section follows the scroll.
  useEffect(() => {
    if (!headings.length) return;
    let frame = 0;
    const update = () => {
      frame = 0;
      const tops = headings.map(
        (h) => document.getElementById(h.id)?.getBoundingClientRect().top ?? Infinity,
      );
      const root = document.scrollingElement!;
      const atEnd = root.scrollTop > 0 && root.scrollTop + innerHeight >= root.scrollHeight - 2;
      const i = activeHeading(tops, innerHeight * 0.2, atEnd);
      setActive(i < 0 ? "" : headings[i].id);
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    update();
    addEventListener("scroll", schedule, { passive: true });
    addEventListener("resize", schedule);
    return () => {
      removeEventListener("scroll", schedule);
      removeEventListener("resize", schedule);
      cancelAnimationFrame(frame);
    };
  }, [headings]);
  const toc = headings.length > 0 && <TocList headings={headings} active={active} />;
  return (
    <div className={cx(styles.layout, toc && styles.withToc)}>
      {toc && (
        <div className={styles.tocRail}>
          <span className={styles.railLabel} aria-hidden="true">
            Contents
          </span>
          <TocList headings={headings} active={active} follow />
        </div>
      )}
      {toc && (
        <details className={styles.tocFold}>
          <summary>Contents</summary>
          {toc}
        </details>
      )}
      <aside className={styles.meta} aria-label="Doc metadata">
        {meta}
      </aside>
      <article ref={body} className={styles.body}>
        {children}
      </article>
    </div>
  );
}

// The contents: one fixed track along the list's left edge with a marker
// that slides to the current entry, so the hint never moves sideways; the
// level indent applies to the text only. `follow` keeps the current entry in
// view when the rail scrolls itself.
function TocList({
  headings,
  active,
  follow = false,
}: {
  headings: Heading[];
  active: string;
  follow?: boolean;
}) {
  const list = useRef<HTMLOListElement>(null);
  const top = Math.min(...headings.map((h) => h.level));
  useLayoutEffect(() => {
    const ol = list.current;
    if (!ol) return;
    const place = () => {
      const a = ol.querySelector<HTMLElement>("a[aria-current]");
      const shown = !!a && a.offsetHeight > 0;
      ol.toggleAttribute("data-marked", shown);
      if (!a || !shown) return;
      ol.style.setProperty("--toc-mark-top", `${a.offsetTop}px`);
      ol.style.setProperty("--toc-mark-height", `${a.offsetHeight}px`);
      const rail = follow ? ol.closest<HTMLElement>(`.${styles.tocRail}`) : null;
      if (rail && rail.scrollHeight > rail.clientHeight) {
        const [r, l] = [rail.getBoundingClientRect(), a.getBoundingClientRect()];
        if (l.top < r.top) rail.scrollTop -= r.top - l.top;
        else if (l.bottom > r.bottom) rail.scrollTop += l.bottom - r.bottom;
      }
    };
    place();
    // The folded list has no size until it opens; a wrap can move entries.
    const seen = new ResizeObserver(place);
    seen.observe(ol);
    return () => seen.disconnect();
  }, [active, headings, follow]);
  return (
    <nav aria-label="Contents">
      <ol ref={list} className={styles.toc}>
        {headings.map((h) => (
          <li key={h.id}>
            <a
              href={`#${encodeURIComponent(h.id)}`}
              data-level={h.level - top}
              className={cx(h.id === active && styles.active)}
              aria-current={h.id === active ? "location" : undefined}
            >
              {h.text}
            </a>
          </li>
        ))}
      </ol>
    </nav>
  );
}
