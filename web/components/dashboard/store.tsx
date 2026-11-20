"use client";
// Store browser: blobs and docs in the axe store, searchable, each opening in
// the store viewer. Store reads need the read session (lib/store.ts).
import { useEffect, useMemo, useState } from "react";
import {
  docScopes,
  formatBytes,
  getJson,
  matches,
  useStoreSession,
  type DocAttachment,
  type StoreBlob,
  type StoreDoc,
} from "../../lib/store";
import {
  Alert,
  Button,
  EmptyState,
  Panel,
  PanelHead,
  Search,
  Skeleton,
  Table,
  Tabs,
  Td,
  Th,
} from "../ui";
import { BlobViewer, DocViewer, StoreUnlock } from "./store-viewer";
import styles from "./store.module.css";

// Rows shown before asking for a narrower search.
const LIMIT = 200;
const day = (s?: string) =>
  s && Number.isFinite(Date.parse(s))
    ? new Date(s).toLocaleDateString(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
      })
    : "—";

type Loaded = {
  blobs: StoreBlob[];
  docs: StoreDoc[];
  attachments: DocAttachment[];
};

export function Store({
  base,
  params,
  onChange,
  onOpenTask,
}: {
  base: string;
  params: URLSearchParams;
  onChange: (patch: Record<string, string | null>) => void;
  onOpenTask: (id: number) => void;
}) {
  const session = useStoreSession(base);
  const kind = params.get("kind") === "docs" ? "docs" : "blobs";
  const query = params.get("q") || "";
  const blobId = params.get("blob");
  const docPath = params.get("doc");
  const [data, setData] = useState<Loaded | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!session.read) return;
    let live = true;
    Promise.all([
      getJson<StoreBlob[]>(`${base}/api/blobs?refs=1`),
      getJson<StoreDoc[]>(`${base}/api/docs`).catch(() => []),
      getJson<DocAttachment[]>(`${base}/api/docs/attachments`).catch(() => []),
    ])
      .then(([blobs, docs, attachments]) => {
        if (!live) return;
        setData({ blobs, docs, attachments });
        setError("");
      })
      .catch((e) => live && setError((e as Error).message));
    return () => {
      live = false;
    };
  }, [base, session.read]);
  const blobs = useMemo(
    () =>
      (data?.blobs || [])
        .filter((b) =>
          matches(
            [b.sha256, b.content_type, ...(b.names || []), ...(b.tasks || []).map((t) => `#${t}`)],
            query,
          ),
        )
        .sort((a, b) => (b.created_at || "").localeCompare(a.created_at || "")),
    [data, query],
  );
  const docs = useMemo(
    () =>
      (data?.docs || [])
        .map((d) => ({ ...d, scopes: docScopes(d.path, data?.attachments || []) }))
        .filter((d) => matches([d.path, ...d.scopes], query))
        .sort((a, b) => a.path.localeCompare(b.path)),
    [data, query],
  );
  if (session.read === null) return <Skeleton label="Checking the store session…" lines={3} />;
  if (!session.read)
    return (
      <Panel aria-label="Store">
        <PanelHead label="Store" />
        <StoreUnlock
          base={base}
          reason="Blobs and docs are private reads. Unlock the store with the server's token to browse them."
        />
      </Panel>
    );
  const selectedBlob = blobId ? data?.blobs.find((b) => b.sha256 === blobId) : undefined;
  const rows = kind === "blobs" ? blobs : docs;
  return (
    <div className={styles.page}>
      {(blobId || docPath) && (
        <Panel aria-label="Store item">
          <PanelHead label={blobId ? "Blob" : "Doc"}>
            <Button variant="ghost" onClick={() => onChange({ blob: null, doc: null })}>
              Close
            </Button>
          </PanelHead>
          {blobId ? (
            !data ? (
              <Skeleton label="Opening…" lines={2} />
            ) : (
              <BlobViewer
                key={blobId}
                base={base}
                sha256={blobId}
                name={selectedBlob?.names?.[0] || ""}
                contentType={selectedBlob?.content_type || undefined}
                size={selectedBlob?.size}
              />
            )
          ) : (
            <DocViewer key={docPath} base={base} path={docPath!} />
          )}
        </Panel>
      )}
      <Panel aria-label="Store">
        <PanelHead label="Store">
          <Button variant="ghost" onClick={() => session.lock()}>
            Lock
          </Button>
        </PanelHead>
        <Tabs
          tabs={["blobs", "docs"]}
          value={kind}
          onChange={(t) => onChange({ kind: t === "blobs" ? null : t })}
        />
        <div className={styles.toolbar}>
          <Search
            compact
            aria-label={`Search ${kind}`}
            placeholder={kind === "blobs" ? "Name, sha256, type or #task" : "Path or label"}
            value={query}
            onChange={(e) => onChange({ q: e.target.value || null })}
          />
          {data && (
            <span role="status">
              {rows.length > LIMIT ? `First ${LIMIT} of ${rows.length}` : rows.length} of{" "}
              {kind === "blobs" ? data.blobs.length : data.docs.length} {kind}
              {kind === "blobs" ? " · Newest first" : ""}
            </span>
          )}
        </div>
        {error ? (
          <Alert>Unable to load the store: {error}</Alert>
        ) : !data ? (
          <Skeleton label="Loading the store…" lines={6} />
        ) : !rows.length ? (
          <EmptyState>{query ? `No ${kind} match.` : `No ${kind} stored.`}</EmptyState>
        ) : kind === "blobs" ? (
          <Table aria-label="Blobs">
            <thead>
              <tr>
                <Th>Name</Th>
                <Th>Type</Th>
                <Th numeric>Size</Th>
                <Th>Stored</Th>
                <Th>Tasks</Th>
              </tr>
            </thead>
            <tbody>
              {blobs.slice(0, LIMIT).map((b) => (
                <tr key={b.sha256} aria-selected={b.sha256 === blobId || undefined}>
                  <Td truncate title={`${(b.names || []).join(", ")} ${b.sha256}`}>
                    <Button
                      variant="link"
                      className={styles.rowButton}
                      onClick={() => onChange({ blob: b.sha256, doc: null })}
                    >
                      {b.names?.[0] || b.sha256.slice(0, 12)}
                    </Button>
                    {(b.names?.length || 0) > 1 && (
                      <span className={styles.meta}> +{b.names!.length - 1}</span>
                    )}
                  </Td>
                  <Td>{(b.content_type || "—").split(";")[0]}</Td>
                  <Td numeric>{formatBytes(b.size)}</Td>
                  <Td>{day(b.created_at)}</Td>
                  <Td>
                    <span className={styles.taskLinks}>
                      {(b.tasks || []).slice(0, 4).map((t) => (
                        <Button key={t} variant="link" onClick={() => onOpenTask(t)}>
                          #{t}
                        </Button>
                      ))}
                      {(b.tasks?.length || 0) > 4 && (
                        <span className={styles.meta}>+{b.tasks!.length - 4}</span>
                      )}
                    </span>
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
        ) : (
          <Table aria-label="Docs">
            <thead>
              <tr>
                <Th>Path</Th>
                <Th>Labels</Th>
                <Th numeric>Size</Th>
                <Th>Updated</Th>
              </tr>
            </thead>
            <tbody>
              {docs.slice(0, LIMIT).map((d) => (
                <tr key={d.path} aria-selected={d.path === docPath || undefined}>
                  <Td truncate title={d.path}>
                    <Button
                      variant="link"
                      className={styles.rowButton}
                      onClick={() => onChange({ doc: d.path, blob: null })}
                    >
                      {d.path}
                    </Button>
                  </Td>
                  <Td>{d.scopes.join(", ") || "—"}</Td>
                  <Td numeric>{formatBytes(d.size)}</Td>
                  <Td>{day(d.updated_at)}</Td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Panel>
    </div>
  );
}
