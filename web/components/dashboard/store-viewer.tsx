"use client";
// Viewer for items in the axe store: blobs (signed links, re-signed by the
// server when expired) and docs. HTML renders in a sandboxed frame with no
// script or same-origin access, and the server also sends
// `Content-Security-Policy: sandbox` with every blob it serves.
import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import Markdown from "../../app/markdown";
import {
  formatBytes,
  getJson,
  previewKind,
  signLink,
  splitFrontmatter,
  storeLink,
  TEXT_PREVIEW_BYTES,
  useStoreSession,
  type PreviewKind,
  type StoreLink,
} from "../../lib/store";
import { Alert, Button, CodeBlock, cx, Input, Skeleton } from "../ui";
import styles from "./store.module.css";

// A full local timestamp, or a dash.
export function fullDate(s?: string | number) {
  if (s === undefined || s === "") return "—";
  const d = new Date(s);
  return Number.isFinite(d.getTime())
    ? d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" })
    : "—";
}

// The item's metadata as label/value rows.
export function Facts({ label, rows }: { label: string; rows: [string, ReactNode][] }) {
  return (
    <dl className={styles.facts} aria-label={label}>
      {rows.map(([k, v]) => (
        <div key={k} className={styles.fact}>
          <dt>{k}</dt>
          <dd>{v}</dd>
        </div>
      ))}
    </dl>
  );
}

// What the store pane knows about a blob beyond its link.
// Rendered markdown; leading YAML frontmatter shows as a small facts block,
// like the metadata above it, not as a bold paragraph.
function MarkdownText({ text }: { text: string }) {
  const { meta, body } = splitFrontmatter(text);
  return (
    <div className={styles.text}>
      {meta.length > 0 && <Facts label="Frontmatter" rows={meta} />}
      <Markdown body={body} compact />
    </div>
  );
}

export type BlobInfo = { names: string[]; tasks: number[]; storedAt?: string };

export function StoreUnlock({ base, reason }: { base: string; reason: string }) {
  const { unlock } = useStoreSession(base);
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      await unlock(token);
      setToken("");
    } catch (err) {
      setError(
        (err as { status?: number }).status === 401
          ? "The server rejected this token."
          : `Unlock failed: ${(err as Error).message}`,
      );
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className={styles.unlock} onSubmit={submit} aria-label="Unlock the store">
      <p>{reason}</p>
      <div className={styles.unlockRow}>
        <Input
          type="password"
          autoComplete="off"
          aria-label="axe token"
          placeholder="axe token"
          value={token}
          onChange={(e) => setToken(e.target.value)}
        />
        <Button type="submit" variant="primary" disabled={busy || !token.trim()}>
          Unlock
        </Button>
      </div>
      <p className={styles.hint}>
        The server swaps the token for a read-only session cookie (7 days) that
        reads blobs and docs; this page does not keep the token.
      </p>
      {error && <Alert>{error}</Alert>}
    </form>
  );
}

type Blob = {
  sha256: string;
  name: string;
  contentType?: string;
  size?: number;
};

// A blob to show: a signed link from an artifact, a stored sha256, or a
// recorded URL the server may map to a blob (`/api/blobs/resolve`).
export function BlobViewer({
  base,
  link,
  sha256,
  name = "",
  contentType,
  size,
  url,
  fill = false,
  info,
  onOpenTask,
}: {
  base: string;
  link?: StoreLink;
  sha256?: string;
  name?: string;
  contentType?: string;
  size?: number;
  url?: string;
  // Fill the containing pane (the store page) instead of a capped box.
  fill?: boolean;
  // Show the blob's metadata (the store page).
  info?: BlobInfo;
  onOpenTask?: (id: number) => void;
}) {
  const session = useStoreSession(base);
  const [blob, setBlob] = useState<Blob | null | "missing">(
    link
      ? { sha256: link.sha256, name: link.name || name }
      : sha256
        ? { sha256, name, contentType, size }
        : null,
  );
  const [signed, setSigned] = useState("");
  const [error, setError] = useState("");
  // Checked once, when the viewer opens.
  const [fresh] = useState(() => !!link && link.expires > Date.now() + 30_000);
  const src = fresh && link ? base + link.path : signed;
  // Resolve a recorded URL to its blob.
  useEffect(() => {
    if (blob || !url || !session.read) return;
    let live = true;
    getJson<{ sha256: string; name?: string; content_type?: string; size?: number }>(
      `${base}/api/blobs/resolve?url=${encodeURIComponent(url)}`,
    )
      .then((r) => {
        if (live)
          setBlob({
            sha256: r.sha256,
            name: r.name || name,
            contentType: r.content_type,
            size: r.size,
          });
      })
      .catch((e) => {
        if (!live) return;
        if ((e as { status?: number }).status === 404) setBlob("missing");
        else setError((e as Error).message);
      });
    return () => {
      live = false;
    };
  }, [base, blob, url, name, session.read]);
  // A readable link: the recorded one while it is valid, else a new one.
  useEffect(() => {
    if (!blob || blob === "missing" || src || !session.read) return;
    let live = true;
    signLink(base, blob.sha256, blob.name)
      .then((path) => live && setSigned(base + path))
      .catch((e) => live && setError((e as Error).message));
    return () => {
      live = false;
    };
  }, [base, blob, src, session.read]);

  if (error) return <Alert>Unable to open this item: {error}</Alert>;
  if (blob === "missing")
    return <p className={styles.note}>This reference is not in the store.</p>;
  if (!blob || !src) {
    if (session.read === false)
      return (
        <StoreUnlock
          base={base}
          reason={
            link
              ? `This link expired ${new Date(link.expires).toLocaleString()}. Unlock the store to re-sign it from its sha256.`
              : "Unlock the store to open this item."
          }
        />
      );
    return <Skeleton label="Opening…" lines={2} />;
  }
  return (
    <BlobPreview
      key={src}
      src={src}
      blob={blob}
      fill={fill}
      info={info}
      onOpenTask={onOpenTask}
    />
  );
}

function BlobPreview({
  src,
  blob,
  fill,
  info,
  onOpenTask,
}: {
  src: string;
  blob: Blob;
  fill: boolean;
  info?: BlobInfo;
  onOpenTask?: (id: number) => void;
}) {
  const [meta, setMeta] = useState<{ kind: PreviewKind; size?: number; type: string } | null>(
    blob.contentType
      ? {
          kind: previewKind(blob.contentType, blob.name),
          size: blob.size,
          type: blob.contentType,
        }
      : null,
  );
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    const needsText = !meta || meta.kind === "text" || meta.kind === "markdown";
    if (!needsText || (meta?.size ?? 0) > TEXT_PREVIEW_BYTES) return;
    const ctl = new AbortController();
    (async () => {
      const r = await fetch(src, { signal: ctl.signal });
      if (!r.ok) throw Error(r.status === 403 ? "the link was refused" : `HTTP ${r.status}`);
      const type = r.headers.get("content-type") || "";
      const size = Number(r.headers.get("content-length") ?? NaN);
      const kind = previewKind(type, blob.name);
      if (!meta) setMeta({ kind, size: Number.isFinite(size) ? size : undefined, type });
      if ((kind === "text" || kind === "markdown") && !(size > TEXT_PREVIEW_BYTES))
        setText(await r.text());
      else ctl.abort();
    })().catch((e) => {
      if (!ctl.signal.aborted) setError((e as Error).message);
    });
    return () => ctl.abort();
    // `meta` is set at most once here; re-running would refetch.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [src, blob.name]);
  const title = blob.name || blob.sha256.slice(0, 12);
  const expires = storeLink(src)?.expires;
  return (
    <div className={cx(styles.viewer, fill && styles.fill)} data-store-viewer>
      <div className={styles.viewerHead}>
        <span className={styles.viewerName} title={blob.name}>
          {title}
        </span>
        {!info && (
          <>
            <span className={styles.meta}>
              {meta ? `${meta.type || "unknown type"}` : "…"}
              {meta?.size !== undefined && ` · ${formatBytes(meta.size)}`}
            </span>
            <span className={styles.sha} title="sha256">
              {blob.sha256}
            </span>
          </>
        )}
        <span className={styles.actions}>
          <a href={src} target="_blank" rel="noopener noreferrer">
            {info ? "Open full" : "Open"}
          </a>
          <a href={src} download={blob.name || blob.sha256}>
            Download
          </a>
        </span>
      </div>
      {info && (
        <Facts
          label="Blob details"
          rows={[
            ["Name", info.names.length ? info.names.join(", ") : "—"],
            ["sha256", <span key="v" className={styles.mono}>{blob.sha256}</span>],
            ["Type", meta ? meta.type || "unknown" : "…"],
            ["Size", meta?.size !== undefined ? formatBytes(meta.size) : "—"],
            ["Stored", fullDate(info.storedAt)],
            [
              "Tasks",
              info.tasks.length ? (
                <span className={styles.taskLinks}>
                  {info.tasks.map((t) =>
                    onOpenTask ? (
                      <Button key={t} variant="link" onClick={() => onOpenTask(t)}>
                        #{t}
                      </Button>
                    ) : (
                      <span key={t}>#{t}</span>
                    ),
                  )}
                </span>
              ) : (
                "None"
              ),
            ],
            ["Link expires", fullDate(expires)],
          ]}
        />
      )}
      {error ? (
        <Alert>Unable to load the preview: {error}</Alert>
      ) : !meta ? (
        <Skeleton label="Loading preview…" lines={3} />
      ) : meta.kind === "image" ? (
        // eslint-disable-next-line @next/next/no-img-element
        <img className={styles.image} src={src} alt={`Preview of ${title}`} />
      ) : meta.kind === "html" ? (
        <iframe
          className={styles.frame}
          sandbox=""
          referrerPolicy="no-referrer"
          src={src}
          title={`Preview of ${title}`}
        />
      ) : (meta.kind === "text" || meta.kind === "markdown") &&
        !((meta.size ?? 0) > TEXT_PREVIEW_BYTES) ? (
        text === null ? (
          <Skeleton label="Loading preview…" lines={3} />
        ) : meta.kind === "markdown" ? (
          <MarkdownText text={text} />
        ) : (
          <CodeBlock className={styles.text}>{text}</CodeBlock>
        )
      ) : (
        <p className={styles.note}>
          No inline preview for this type{meta.kind !== "download" && " at this size"}; use
          Download.
        </p>
      )}
    </div>
  );
}

export type DocBody = {
  path: string;
  version: string;
  kind: "doc" | "folder";
  content?: string;
  files?: { path: string; content: string }[];
  // The store's content type for a single doc, when it records one.
  content_type?: string;
};

// What the store pane knows about a doc from the listing.
export type DocInfo = { size: number; updatedAt: string; scopes: string[] };
// A `/api/docs/log` entry: the commit, its time, the writer and the
// write's `-m` message.
export type DocVersion = { version: string; at: string; author?: string; message?: string };

// The doc's full page (app/doc): this dashboard, pointed at the same server.
export function docPageHref(base: string, path: string) {
  const q = new URLSearchParams({ path });
  if (typeof location !== "undefined" && base !== location.origin) q.set("server", base);
  return `/doc?${q}`;
}

export function DocViewer({
  base,
  path,
  fill = false,
  info,
}: {
  base: string;
  path: string;
  fill?: boolean;
  // Show the doc's metadata and a link to its full page (the store page).
  info?: DocInfo;
}) {
  const session = useStoreSession(base);
  const [doc, setDoc] = useState<DocBody | null>(null);
  const [versions, setVersions] = useState<DocVersion[] | null>(null);
  const [error, setError] = useState("");
  const details = !!info;
  useEffect(() => {
    if (!session.read) return;
    let live = true;
    getJson<DocBody>(`${base}/api/docs/file?path=${encodeURIComponent(path)}`)
      .then((d) => live && setDoc(d))
      .catch((e) => live && setError((e as Error).message));
    if (details)
      getJson<DocVersion[]>(`${base}/api/docs/log?path=${encodeURIComponent(path)}`)
        .then((v) => live && setVersions(v))
        .catch(() => live && setVersions([]));
    return () => {
      live = false;
    };
  }, [base, path, session.read, details]);
  if (session.read === false)
    return <StoreUnlock base={base} reason="Unlock the store to read docs." />;
  if (error) return <Alert>Unable to open this doc: {error}</Alert>;
  if (!doc) return <Skeleton label="Opening…" lines={3} />;
  const files =
    doc.kind === "folder" ? doc.files || [] : [{ path: "", content: doc.content || "" }];
  return (
    <div className={cx(styles.viewer, fill && styles.fill)} data-store-viewer>
      <div className={styles.viewerHead}>
        <span className={styles.viewerName}>{doc.path}</span>
        {!info && (
          <span className={styles.meta}>
            {doc.kind} · version <span className={styles.mono}>{doc.version.slice(0, 12)}</span>
          </span>
        )}
        {info && (
          <span className={styles.actions}>
            <a href={docPageHref(base, doc.path)} target="_blank" rel="noopener noreferrer">
              Open full
            </a>
          </span>
        )}
      </div>
      {info && (
        <Facts
          label="Doc details"
          rows={[
            ["Path", <span key="v" className={styles.mono}>{doc.path}</span>],
            ["Kind", doc.kind],
            ["Version", <span key="v" className={styles.mono}>{doc.version}</span>],
            [
              "Versions",
              versions === null
                ? "…"
                : versions.length
                  ? `${versions.length} · first ${fullDate(versions[versions.length - 1].at)}`
                  : "—",
            ],
            ["Size", formatBytes(info.size)],
            ["Updated", fullDate(info.updatedAt)],
            ["Scopes", info.scopes.length ? info.scopes.join(", ") : "None"],
          ]}
        />
      )}
      {files.map((f) => (
        <section key={f.path} aria-label={f.path || doc.path} className={styles.docFile}>
          {f.path && <h4 className={styles.mono}>{f.path}</h4>}
          {/\.md$/i.test(f.path || doc.path) ? (
            <MarkdownText text={f.content} />
          ) : (
            <CodeBlock className={styles.text}>{f.content}</CodeBlock>
          )}
        </section>
      ))}
    </div>
  );
}
