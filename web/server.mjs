import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { resolve, extname } from "node:path";
const root = resolve(process.env.STATIC_DIR || "out");
const mime = {
  ".html": "text/html",
  ".js": "application/javascript",
  ".css": "text/css",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
  ".txt": "text/plain",
};
createServer(async (req, res) => {
  const path = new URL(req.url, "http://localhost").pathname;
  if (!["GET", "HEAD"].includes(req.method)) {
    res.writeHead(405);
    res.end();
    return;
  }
  if (path === "/healthz" || path === "/api/version") {
    res.setHeader("Content-Type", "application/json");
    res.end(
      JSON.stringify(
        path === "/healthz"
          ? { ok: true }
          : { commit: process.env.GIT_COMMIT || "development" },
      ),
    );
    return;
  }
  let decoded;
  try {
    decoded = decodeURIComponent(path === "/" ? "/index.html" : path);
  } catch {
    res.writeHead(400);
    res.end("Malformed URL");
    return;
  }
  const file = resolve(root, "." + decoded);
  if (!file.startsWith(root + "/")) {
    res.writeHead(403);
    res.end();
    return;
  }
  try {
    const data = await readFile(file);
    res.setHeader(
      "Content-Type",
      mime[extname(file)] || "application/octet-stream",
    );
    res.end(req.method === "HEAD" ? undefined : data);
  } catch {
    res.writeHead(404);
    res.end("Not found");
  }
}).listen(Number(process.env.PORT || 8080), "0.0.0.0");
