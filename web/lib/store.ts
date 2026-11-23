// The axe store as the dashboard reads it: signed blob links, preview kinds,
// and the read session that unlocks blob and doc reads (server.rs
// `session_route`; the cookie is HttpOnly, so the page never keeps the token).
import { useCallback, useEffect, useState } from "react";

export type StoreLink = {
  sha256: string;
  name: string;
  // Signed path on the server (`/b/<sha>[/<name>]?exp=..&sig=..`).
  path: string;
  // Expiry, epoch milliseconds.
  expires: number;
};

const SHA = /^[0-9a-f]{64}$/;

// A signed store link (any host: the signature names only the blob, file
// name and expiry), or undefined.
export function storeLink(href: string): StoreLink | undefined {
  let url: URL;
  try {
    url = new URL(href);
  } catch {
    return undefined;
  }
  const parts = url.pathname.split("/").filter(Boolean);
  const exp = Number(url.searchParams.get("exp"));
  if (
    parts[0] !== "b" ||
    !SHA.test(parts[1] || "") ||
    parts.length > 3 ||
    !url.searchParams.get("sig") ||
    !Number.isFinite(exp)
  )
    return undefined;
  let name = parts[2] || "";
  try {
    name = decodeURIComponent(name);
  } catch {
    /* Keep the encoded name. */
  }
  return {
    sha256: parts[1],
    name,
    path: url.pathname + url.search,
    expires: exp * 1000,
  };
}

export type PreviewKind = "image" | "html" | "markdown" | "text" | "download";

// How the viewer shows a blob: by content type, else by file extension.
export function previewKind(contentType: string, name = ""): PreviewKind {
  const ct = contentType.split(";")[0].trim().toLowerCase();
  const ext = (name.match(/\.([a-z0-9]{1,10})$/i)?.[1] || "").toLowerCase();
  if (ct === "text/html" || (!ct && ["html", "htm"].includes(ext)))
    return "html";
  if (ct === "text/markdown" || ext === "md") return "markdown";
  if (ct.startsWith("image/")) return "image";
  if (
    ct.startsWith("text/") ||
    ["application/json", "application/x-ndjson", "application/xml"].includes(ct)
  )
    return "text";
  if (!ct || ct === "application/octet-stream") {
    if (["png", "jpg", "jpeg", "gif", "webp", "svg"].includes(ext))
      return "image";
    if (["html", "htm"].includes(ext)) return "html";
    if (["txt", "log", "json", "csv", "tsv", "yaml", "yml", "toml", "diff", "patch"].includes(ext))
      return "text";
  }
  return "download";
}

// Text previews stop here; larger files download.
export const TEXT_PREVIEW_BYTES = 1024 * 1024;

export function formatBytes(n: number) {
  if (!Number.isFinite(n) || n < 0) return "—";
  if (n < 1024) return `${n} B`;
  const units = ["KiB", "MiB", "GiB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

// `GET /api/blobs?refs=1` rows.
export type StoreBlob = {
  sha256: string;
  size: number;
  content_type?: string;
  created_at?: string;
  names?: string[];
  tasks?: number[];
};
// `GET /api/docs` rows.
export type StoreDoc = {
  path: string;
  size: number;
  version: string;
  updated_at: string;
};
// `GET /api/docs/attachments` rows.
export type DocAttachment = { scope: string; path: string; exists: boolean };

// Scopes a doc is attached under: its own path or a folder above it.
export function docScopes(path: string, attachments: DocAttachment[]) {
  return Array.from(
    new Set(
      attachments
        .filter((a) => path === a.path || path.startsWith(a.path.replace(/\/+$/, "") + "/"))
        .map((a) => a.scope),
    ),
  ).sort();
}

export function matches(haystack: (string | number | undefined)[], query: string) {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const text = haystack.filter((v) => v !== undefined).join(" ").toLowerCase();
  return words.every((w) => text.includes(w));
}

export async function getJson<T>(url: string, init?: RequestInit): Promise<T> {
  const r = await fetch(url, init);
  if (!r.ok) {
    let msg = `HTTP ${r.status}`;
    try {
      const e = await r.json();
      if (e?.error) msg += `: ${e.error}`;
    } catch {
      /* Not JSON. */
    }
    throw Object.assign(Error(msg), { status: r.status });
  }
  return r.json();
}

// A fresh signed path for a blob, made by the server (needs a read session).
export async function signLink(base: string, sha: string, name: string, ttl = 3600) {
  const r = await getJson<{ path: string }>(`${base}/api/blobs/${sha}/link`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ttl, name: name && !name.includes("/") ? name : undefined }),
  });
  return r.path;
}

// --- read session, shared by every component on the page ---

type Session = { read: boolean; expires_at?: string | null } | null;
const sessions = new Map<string, Session>();
const listeners = new Set<() => void>();
const loading = new Map<string, Promise<void>>();
function publish(base: string, s: Session) {
  sessions.set(base, s);
  listeners.forEach((l) => l());
}
function refreshSession(base: string) {
  if (!loading.has(base))
    loading.set(
      base,
      getJson<Session>(`${base}/api/session`)
        .then((s) => publish(base, s))
        .catch(() => publish(base, { read: false }))
        .finally(() => loading.delete(base)),
    );
  return loading.get(base)!;
}

export function useStoreSession(base: string) {
  const [, setTick] = useState(0);
  useEffect(() => {
    const l = () => setTick((n) => n + 1);
    listeners.add(l);
    if (!sessions.has(base)) refreshSession(base);
    return () => {
      listeners.delete(l);
    };
  }, [base]);
  const unlock = useCallback(
    async (token: string) => {
      const s = await getJson<Session>(`${base}/api/session`, {
        method: "POST",
        headers: { Authorization: `Bearer ${token.trim()}` },
      });
      publish(base, s);
    },
    [base],
  );
  const lock = useCallback(async () => {
    await getJson<Session>(`${base}/api/session-end`, { method: "POST" }).catch(() => null);
    publish(base, { read: false });
  }, [base]);
  const s = sessions.get(base);
  return {
    // null while the first check runs.
    read: s ? s.read : null,
    expiresAt: s?.expires_at || null,
    unlock,
    lock,
  };
}

// A compact date for one-line table cells ("Nov 23"; the year only when it
// is not this year), with the full local timestamp for a tooltip.
export function shortDate(s?: string, now = new Date()) {
  if (!s || !Number.isFinite(Date.parse(s))) return { text: "—", title: undefined };
  const d = new Date(s);
  return {
    text: d.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      ...(d.getFullYear() === now.getFullYear() ? {} : { year: "numeric" }),
    }),
    title: d.toLocaleString(undefined, { dateStyle: "full", timeStyle: "long" }),
  };
}

// The docs list as a folder tree, from `/`-separated path segments. A path
// that is both a doc and a folder above other docs is one node with both.
export type DocNode<T> = { name: string; path: string; doc?: T; children: DocNode<T>[] };
export function docTree<T extends { path: string }>(docs: T[]): DocNode<T>[] {
  const root: DocNode<T> = { name: "", path: "", children: [] };
  for (const d of docs) {
    let node = root;
    const parts = d.path.split("/").filter(Boolean);
    parts.forEach((name, i) => {
      const path = parts.slice(0, i + 1).join("/");
      let next = node.children.find((c) => c.name === name);
      if (!next) node.children.push((next = { name, path, children: [] }));
      node = next;
    });
    if (node !== root) node.doc = d;
  }
  const sort = (n: DocNode<T>) => {
    // Folders first, then files, each by name.
    n.children.sort(
      (a, b) =>
        Number(!a.children.length) - Number(!b.children.length) || a.name.localeCompare(b.name),
    );
    n.children.forEach(sort);
  };
  sort(root);
  return root.children;
}

// The folders above a path, outermost first.
export function parentFolders(path: string) {
  const parts = path.split("/").filter(Boolean);
  return parts.slice(0, -1).map((_, i) => parts.slice(0, i + 1).join("/"));
}
