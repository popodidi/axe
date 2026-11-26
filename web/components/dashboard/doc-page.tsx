"use client";
// A doc's full page (app/doc): a table of contents for a long markdown or
// HTML doc (components/ui/toc.tsx) in a sticky left rail, its metadata in a sticky right rail, the body in a wide center
// column. Narrower windows fold the contents into a "Contents" disclosure
// first, then stack the metadata above the body. The page scrolls with the
// window; a rail scrolls itself only when it is taller than the window.
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import Markdown from "../../app/markdown";
import { tocEntries } from "../../lib/doc-toc";
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
import { Alert, cx, Skeleton, TocFold, TocRail, useHeadings, useScrollSpy } from "../ui";
import { DocFile, type HtmlToc } from "./doc-file";
import { Facts, fullDate, StoreUnlock, type DocBody, type DocVersion } from "./store-viewer";
import styles from "./doc-page.module.css";

// Markdown (lib/doc-render.ts) gets frontmatter in the metadata rail;
// markdown and a single HTML doc get contents.
const isMarkdown = (path: string, content: string, contentType?: string) =>
  docKind(path, content, contentType) === "markdown";

// The store's listing row (null when the server lists none) and the scopes
// the doc is attached under.
type StoreFacts = { row: StoreDoc | null; scopes: string[] };

export function DocPage({ base, path }: { base: string; path: string }) {
  const session = useStoreSession(base);
  const [doc, setDoc] = useState<DocBody | null>(null);
  const [versions, setVersions] = useState<DocVersion[] | null>(null);
  const [store, setStore] = useState<StoreFacts | null>(null);
  const [error, setError] = useState("");
  const [htmlToc, setHtmlToc] = useState<HtmlToc | null>(null);
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
      htmlToc={single ? htmlToc : null}
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
            onHtmlToc={single ? setHtmlToc : undefined}
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

const NO_ENTRIES: ReturnType<typeof tocEntries> = [];

function DocLayout({
  meta,
  markdown,
  htmlToc,
  bodyKey,
  children,
}: {
  meta: ReactNode;
  markdown: boolean;
  htmlToc: HtmlToc | null;
  bodyKey: string;
  children: ReactNode;
}) {
  const body = useRef<HTMLElement>(null);
  // A markdown body is the source of its contents: its h1-h3 with ids.
  const headings = useHeadings(body, bodyKey, markdown);
  const entries = useMemo(() => tocEntries(headings), [headings]);
  // The browser looked for the anchor before the doc loaded.
  useEffect(() => {
    const id = decodeURIComponent(location.hash.slice(1));
    if (id && headings.length) document.getElementById(id)?.scrollIntoView();
  }, [headings]);
  // The current section follows the window's scroll; an HTML doc's frame
  // reports its own.
  const spied = useScrollSpy(htmlToc ? NO_ENTRIES : entries);
  const toc = htmlToc
    ? { entries: htmlToc.entries, active: htmlToc.active, onJump: htmlToc.jump }
    : { entries, active: spied };
  const shown = toc.entries.length > 0;
  return (
    <div className={cx(styles.layout, shown && styles.withToc)}>
      <TocRail {...toc} fixed className={styles.tocRail} />
      <TocFold {...toc} className={styles.tocFold} />
      <aside className={styles.meta} aria-label="Doc metadata">
        {meta}
      </aside>
      <article ref={body} className={styles.body}>
        {children}
      </article>
    </div>
  );
}
