"use client";
// Viewer for items in the axe store: blobs (signed links, re-signed by the
// server when expired) and docs. HTML renders in a sandboxed frame with no
// script or same-origin access, and the server also sends
// `Content-Security-Policy: sandbox` with every blob it serves.
import { useEffect, useState, type FormEvent } from "react";
import Markdown from "../../app/markdown";
import {
  formatBytes,
  getJson,
  previewKind,
  signLink,
  TEXT_PREVIEW_BYTES,
  useStoreSession,
  type PreviewKind,
  type StoreLink,
} from "../../lib/store";
import { Alert, Button, CodeBlock, Input, Skeleton } from "../ui";
import styles from "./store.module.css";

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
}: {
  base: string;
  link?: StoreLink;
  sha256?: string;
  name?: string;
  contentType?: string;
  size?: number;
  url?: string;
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
  return <BlobPreview key={src} src={src} blob={blob} />;
}

function BlobPreview({ src, blob }: { src: string; blob: Blob }) {
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
  return (
    <div className={styles.viewer} data-store-viewer>
      <div className={styles.viewerHead}>
        <span className={styles.viewerName} title={blob.name}>
          {title}
        </span>
        <span className={styles.meta}>
          {meta ? `${meta.type || "unknown type"}` : "…"}
          {meta?.size !== undefined && ` · ${formatBytes(meta.size)}`}
        </span>
        <span className={styles.sha} title="sha256">
          {blob.sha256}
        </span>
        <span className={styles.actions}>
          <a href={src} target="_blank" rel="noopener noreferrer">
            Open
          </a>
          <a href={src} download={blob.name || blob.sha256}>
            Download
          </a>
        </span>
      </div>
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
          <div className={styles.text}>
            <Markdown body={text} compact />
          </div>
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

type DocBody = {
  path: string;
  version: string;
  kind: "doc" | "folder";
  content?: string;
  files?: { path: string; content: string }[];
};

export function DocViewer({ base, path }: { base: string; path: string }) {
  const session = useStoreSession(base);
  const [doc, setDoc] = useState<DocBody | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!session.read) return;
    let live = true;
    getJson<DocBody>(`${base}/api/docs/file?path=${encodeURIComponent(path)}`)
      .then((d) => live && setDoc(d))
      .catch((e) => live && setError((e as Error).message));
    return () => {
      live = false;
    };
  }, [base, path, session.read]);
  if (session.read === false)
    return <StoreUnlock base={base} reason="Unlock the store to read docs." />;
  if (error) return <Alert>Unable to open this doc: {error}</Alert>;
  if (!doc) return <Skeleton label="Opening…" lines={3} />;
  const files =
    doc.kind === "folder" ? doc.files || [] : [{ path: "", content: doc.content || "" }];
  return (
    <div className={styles.viewer} data-store-viewer>
      <div className={styles.viewerHead}>
        <span className={styles.viewerName}>{doc.path}</span>
        <span className={styles.meta}>
          {doc.kind} · version <span className={styles.mono}>{doc.version.slice(0, 12)}</span>
        </span>
      </div>
      {files.map((f) => (
        <section key={f.path} aria-label={f.path || doc.path} className={styles.docFile}>
          {f.path && <h4 className={styles.mono}>{f.path}</h4>}
          {/\.md$/i.test(f.path || doc.path) ? (
            <div className={styles.text}>
              <Markdown body={f.content} compact />
            </div>
          ) : (
            <CodeBlock className={styles.text}>{f.content}</CodeBlock>
          )}
        </section>
      ))}
    </div>
  );
}
