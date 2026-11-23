"use client";
// Store browser: blobs in a table and docs in a folder tree, searchable. A
// selected item opens in the dashboard's detail pane (StoreItem). Store reads
// need the read session (lib/store.ts).
import { useEffect, useMemo, useState } from "react";
import {
  countOf,
  docScopes,
  docTree,
  formatBytes,
  getJson,
  matches,
  parentFolders,
  shortDate,
  useStoreSession,
  type DocAttachment,
  type DocNode,
  type StoreBlob,
  type StoreDoc,
  type StoreStats,
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
import { Kpi } from "./stats";
import kpi from "./stats.module.css";
import styles from "./store.module.css";

// Blob rows shown before asking for a narrower search.
const LIMIT = 200;

type Loaded = {
  blobs: StoreBlob[];
  docs: StoreDoc[];
  attachments: DocAttachment[];
  // Null when the server has no stats endpoint or it failed.
  stats: StoreStats | null;
};
type ScopedDoc = StoreDoc & { scopes: string[] };

// The store listing, shared by the list and the detail pane.
export type StoreState = { data: Loaded | null; error: string };
export function useStoreData(base: string, enabled: boolean): StoreState {
  const session = useStoreSession(base);
  const [data, setData] = useState<Loaded | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!enabled || !session.read) return;
    let live = true;
    Promise.all([
      getJson<StoreBlob[]>(`${base}/api/blobs?refs=1`),
      getJson<StoreDoc[]>(`${base}/api/docs`).catch(() => []),
      getJson<DocAttachment[]>(`${base}/api/docs/attachments`).catch(() => []),
      getJson<StoreStats>(`${base}/api/store/stats`).catch(() => null),
    ])
      .then(([blobs, docs, attachments, stats]) => {
        if (!live) return;
        setData({ blobs, docs, attachments, stats });
        setError("");
      })
      .catch((e) => live && setError((e as Error).message));
    return () => {
      live = false;
    };
  }, [base, enabled, session.read]);
  return { data, error };
}

// A selected store item, as `blob:<sha256>` or `doc:<path>` (the pane key).
export function storeItemKey(params: URLSearchParams) {
  const blob = params.get("blob");
  const doc = params.get("doc");
  return blob ? `blob:${blob}` : doc ? `doc:${doc}` : null;
}

// The detail pane body for a store item key.
export function StoreItem({
  base,
  item,
  state,
}: {
  base: string;
  item: string;
  state: StoreState;
}) {
  const id = item.slice(item.indexOf(":") + 1);
  if (item.startsWith("doc:")) return <DocViewer key={id} base={base} path={id} />;
  if (state.error) return <Alert>Unable to load the store: {state.error}</Alert>;
  if (!state.data) return <Skeleton label="Opening…" lines={2} />;
  const blob = state.data.blobs.find((b) => b.sha256 === id);
  return (
    <BlobViewer
      key={id}
      base={base}
      sha256={id}
      name={blob?.names?.[0] || ""}
      contentType={blob?.content_type || undefined}
      size={blob?.size}
    />
  );
}

export function Store({
  base,
  params,
  state: { data, error },
  onChange,
  onOpenTask,
}: {
  base: string;
  params: URLSearchParams;
  state: StoreState;
  onChange: (patch: Record<string, string | null>) => void;
  onOpenTask: (id: number) => void;
}) {
  const session = useStoreSession(base);
  const kind = params.get("kind") === "docs" ? "docs" : "blobs";
  const query = params.get("q") || "";
  const blobId = params.get("blob");
  const docPath = params.get("doc");
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
        .filter((d) => matches([d.path, ...d.scopes], query)),
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
  const count = kind === "blobs" ? blobs.length : docs.length;
  return (
    <Panel aria-label="Store">
      <PanelHead label="Store">
        <Button variant="ghost" onClick={() => session.lock()}>
          Lock
        </Button>
      </PanelHead>
      {data?.stats && <StoreStatsRow stats={data.stats} />}
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
            {kind === "blobs" && count > LIMIT ? `First ${LIMIT} of ${count}` : count} of{" "}
            {kind === "blobs" ? data.blobs.length : data.docs.length} {kind}
            {kind === "blobs" ? " · Newest first" : ""}
          </span>
        )}
      </div>
      {error ? (
        <Alert>Unable to load the store: {error}</Alert>
      ) : !data ? (
        <Skeleton label="Loading the store…" lines={6} />
      ) : !count ? (
        <EmptyState>{query ? `No ${kind} match.` : `No ${kind} stored.`}</EmptyState>
      ) : kind === "blobs" ? (
        <Table aria-label="Blobs" className={styles.blobs}>
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
            {blobs.slice(0, LIMIT).map((b) => {
              const type = (b.content_type || "—").split(";")[0];
              const stored = shortDate(b.created_at);
              return (
                <tr key={b.sha256} aria-selected={b.sha256 === blobId || undefined}>
                  <Td truncate title={`${(b.names || []).join(", ")} ${b.sha256}`}>
                    <Button
                      variant="link"
                      className={styles.rowButton}
                      data-store-item={`blob:${b.sha256}`}
                      onClick={() => onChange({ blob: b.sha256, doc: null })}
                    >
                      {b.names?.[0] || b.sha256.slice(0, 12)}
                    </Button>
                    {(b.names?.length || 0) > 1 && (
                      <span className={styles.meta}> +{b.names!.length - 1}</span>
                    )}
                  </Td>
                  <Td className={styles.type} title={b.content_type || undefined}>
                    {type}
                  </Td>
                  <Td numeric>{formatBytes(b.size)}</Td>
                  <Td className={styles.fit} title={stored.title}>
                    {stored.text}
                  </Td>
                  <Td className={styles.fit}>
                    <span className={styles.taskLinks}>
                      {(b.tasks || []).slice(0, 3).map((t) => (
                        <Button key={t} variant="link" onClick={() => onOpenTask(t)}>
                          #{t}
                        </Button>
                      ))}
                      {(b.tasks?.length || 0) > 3 && (
                        <span
                          className={styles.meta}
                          title={b.tasks!.slice(3).map((t) => `#${t}`).join(" ")}
                        >
                          +{b.tasks!.length - 3}
                        </span>
                      )}
                    </span>
                  </Td>
                </tr>
              );
            })}
          </tbody>
        </Table>
      ) : (
        <DocsTree
          key={query ? "search" : "browse"}
          docs={docs}
          searching={!!query}
          selected={docPath}
          onOpen={(path) => onChange({ doc: path, blob: null })}
        />
      )}
    </Panel>
  );
}

// The store at a glance, from the server's sums (not the listed rows).
function StoreStatsRow({ stats }: { stats: StoreStats }) {
  const types = stats.types.map((t) => `${t.content_type} ${formatBytes(t.bytes)}`);
  if (stats.other_types.count) types.push(`other ${formatBytes(stats.other_types.bytes)}`);
  return (
    <section className={kpi.kpis} aria-label="Store stats">
      <Kpi
        label="Blobs"
        value={formatBytes(stats.total.bytes)}
        detail={countOf(stats.total.count, "blob")}
      />
      <Kpi
        label={`Added in ${stats.recent_days} days`}
        value={formatBytes(stats.recent.bytes)}
        detail={countOf(stats.recent.count, "blob")}
      />
      <div className={kpi.kpi}>
        <div className={kpi.kpiLabel}>Size by type</div>
        <ul className={styles.statTypes}>
          {types.length ? types.map((t) => <li key={t}>{t}</li>) : <li>—</li>}
        </ul>
      </div>
      <Kpi
        label="Not linked by a task"
        value={stats.unreferenced.count.toLocaleString("en-US")}
        detail={`${formatBytes(stats.unreferenced.bytes)} no task event links`}
      />
      {stats.docs && (
        <Kpi
          label="Docs"
          value={stats.docs.count.toLocaleString("en-US")}
          detail={formatBytes(stats.docs.bytes)}
        />
      )}
    </section>
  );
}

// Docs as collapsible folders. Browsing opens the top level and the folders
// above the selected doc; a search opens every folder holding a match.
function DocsTree({
  docs,
  searching,
  selected,
  onOpen,
}: {
  docs: ScopedDoc[];
  searching: boolean;
  selected: string | null;
  onOpen: (path: string) => void;
}) {
  const tree = useMemo(() => docTree(docs), [docs]);
  // Folders the user toggled away from the default: collapsed ones while
  // searching (every folder holding a match starts open), expanded ones while
  // browsing. The caller remounts the tree when it switches between the two.
  const [toggled, setToggled] = useState(
    () =>
      new Set(
        searching ? [] : [...tree.map((n) => n.path), ...(selected ? parentFolders(selected) : [])],
      ),
  );
  const isOpen = (path: string) => toggled.has(path) !== searching;
  const toggle = (path: string) =>
    setToggled((s) => {
      const next = new Set(s);
      if (!next.delete(path)) next.add(path);
      return next;
    });
  const render = (nodes: DocNode<ScopedDoc>[]) =>
    nodes.map((n) => {
      const doc = n.doc;
      const updated = doc && shortDate(doc.updated_at);
      const file = doc && (
        <div className={styles.treeRow} aria-current={doc.path === selected || undefined}>
          <Button
            variant="link"
            className={styles.rowButton}
            title={doc.path}
            data-store-item={`doc:${doc.path}`}
            onClick={() => onOpen(doc.path)}
          >
            {n.name}
          </Button>
          <span className={styles.treeMeta}>
            {doc.scopes.length > 0 && <span>{doc.scopes.join(", ")}</span>}
            <span>{formatBytes(doc.size)}</span>
            <span title={updated?.title}>{updated?.text}</span>
          </span>
        </div>
      );
      if (!n.children.length) return <li key={n.path}>{file}</li>;
      const expanded = isOpen(n.path);
      return (
        <li key={n.path}>
          <Button
            variant="ghost"
            className={styles.folder}
            aria-expanded={expanded}
            title={n.path}
            onClick={() => toggle(n.path)}
          >
            <span className={styles.chevron} aria-hidden="true" />
            {n.name}/
          </Button>
          {expanded && (
            <ul className={styles.treeGroup}>
              {file && <li>{file}</li>}
              {render(n.children)}
            </ul>
          )}
        </li>
      );
    });
  return (
    <ul className={styles.tree} aria-label="Docs">
      {render(tree)}
    </ul>
  );
}
