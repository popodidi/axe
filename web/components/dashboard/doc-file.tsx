"use client";
// One stored file on the doc page, by kind (lib/doc-render.ts): markdown,
// HTML in a sandboxed frame with a Source toggle, highlighted code with line
// numbers, images, CSV/TSV tables, and plain text for anything else. A
// renderer that throws falls back to the plain text, never a blank body.
import { Component, useMemo, useState, type ReactNode } from "react";
import {
  codeLanguage,
  docKind,
  highlightLines,
  imageSrc,
  parseDelimited,
  prettyJson,
} from "../../lib/doc-render";
import { CodeBlock, Table, Tabs } from "../ui";
import styles from "./doc-file.module.css";

export function DocFile({
  path,
  content,
  contentType,
  markdown,
}: {
  path: string;
  content: string;
  contentType?: string;
  // How a markdown file renders (the page keeps its frontmatter handling).
  markdown: (text: string) => ReactNode;
}) {
  const kind = docKind(path, content, contentType);
  return (
    <Fallback key={`${path}:${kind}`} text={content}>
      <div data-doc-kind={kind}>
        {kind === "markdown" ? (
          markdown(content)
        ) : kind === "html" ? (
          <HtmlDoc path={path} html={content} />
        ) : kind === "code" ? (
          <CodeFile path={path} text={content} contentType={contentType} />
        ) : kind === "image" ? (
          // eslint-disable-next-line @next/next/no-img-element
          <img className={styles.image} src={imageSrc(path, content, contentType)!} alt={path} />
        ) : kind === "table" ? (
          <DelimitedTable path={path} text={content} contentType={contentType} />
        ) : kind === "binary" ? (
          <Binary path={path} content={content} />
        ) : (
          <PlainText text={content} />
        )}
      </div>
    </Fallback>
  );
}

export function PlainText({ text }: { text: string }) {
  return <CodeBlock className={styles.plain}>{text}</CodeBlock>;
}

// A render error shows the file as plain text instead.
class Fallback extends Component<{ text: string; children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  render() {
    return this.state.failed ? (
      <div data-doc-kind="text">
        <PlainText text={this.props.text} />
      </div>
    ) : (
      this.props.children
    );
  }
}

function CodeFile({
  path,
  text,
  contentType,
}: {
  path: string;
  text: string;
  contentType?: string;
}) {
  const language = codeLanguage(path, contentType);
  const lines = useMemo(
    () =>
      highlightLines(
        language === "json" && !/\.(jsonl|ndjson)$/i.test(path) ? prettyJson(text) : text,
        language,
      ),
    [path, text, language],
  );
  return (
    <CodeBlock className={styles.code} data-language={language}>
      <code>
        {lines.map((line, i) => (
          <span key={i} className={styles.line}>
            {line.map((t, j) =>
              t.className ? (
                <span key={j} className={t.className}>
                  {t.text}
                </span>
              ) : (
                t.text
              ),
            )}
            {"\n"}
          </span>
        ))}
      </code>
    </CodeBlock>
  );
}

// Large tables stop here, with a note.
const TABLE_ROWS = 2000;

function DelimitedTable({
  path,
  text,
  contentType,
}: {
  path: string;
  text: string;
  contentType?: string;
}) {
  const tsv = /tab-separated/i.test(contentType || "") || /\.tsv$/i.test(path);
  const rows = useMemo(() => parseDelimited(text, tsv ? "\t" : ","), [text, tsv]);
  if (!rows.length) return <PlainText text={text} />;
  const [head, ...body] = rows;
  const width = Math.max(...rows.map((r) => r.length));
  const pad = (r: string[]) => [...r, ...Array(width - r.length).fill("")];
  return (
    <>
      <Table className={styles.table}>
        <thead>
          <tr>
            {pad(head).map((c, i) => (
              <th key={i}>{c}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {body.slice(0, TABLE_ROWS).map((r, i) => (
            <tr key={i}>
              {pad(r).map((c, j) => (
                <td key={j}>{c}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </Table>
      {body.length > TABLE_ROWS && (
        <p className={styles.note}>
          Showing the first {TABLE_ROWS} of {body.length} rows.
        </p>
      )}
    </>
  );
}

function Binary({ path, content }: { path: string; content: string }) {
  return (
    <p className={styles.note}>
      Binary content, no inline preview.{" "}
      <a
        href="#"
        download={path.slice(path.lastIndexOf("/") + 1) || "doc"}
        // The file is made on click and released once the download starts.
        onClick={(e) => {
          const url = URL.createObjectURL(
            new Blob([content], { type: "application/octet-stream" }),
          );
          e.currentTarget.href = url;
          setTimeout(() => URL.revokeObjectURL(url), 0);
        }}
      >
        Download
      </a>
    </p>
  );
}

// Stored HTML renders in a frame with no scripts and an opaque origin
// (sandbox without allow-scripts or allow-same-origin), so it can never run
// code or reach the app's cookies and storage. The markup is also cleaned
// first: scripts, base and refresh tags go, a CSP forbids scripts, and
// relative links and images (which have nothing to resolve against) become
// inert. Links that do resolve open in a new tab.
function HtmlDoc({ path, html }: { path: string; html: string }) {
  const [view, setView] = useState("Rendered");
  const doc = useMemo(() => safeHtml(html), [html]);
  return (
    <div className={styles.html}>
      <Tabs
        tabs={["Rendered", "Source"]}
        value={view}
        onChange={setView}
        className={styles.toggle}
      />
      {view === "Rendered" ? (
        <iframe
          className={styles.frame}
          sandbox="allow-popups allow-popups-to-escape-sandbox"
          referrerPolicy="no-referrer"
          srcDoc={doc}
          title={`Rendered ${path}`}
        />
      ) : (
        <CodeFile path="source.html" text={html} />
      )}
    </div>
  );
}

const CSP =
  "default-src 'none'; img-src data: http: https:; style-src 'unsafe-inline' data: http: https:; font-src data: http: https:; media-src data: http: https:";

// Absolute (or in-page) URLs keep working; anything else would resolve
// against the app and is dropped.
const resolvable = (url: string) => /^(https?:|data:|mailto:|#)/i.test(url.trim());

export function safeHtml(html: string): string {
  const d = new DOMParser().parseFromString(html, "text/html");
  d.querySelectorAll("script, noscript, base, object, embed, iframe, frame, frameset").forEach(
    (e) => e.remove(),
  );
  d.querySelectorAll("meta[http-equiv]").forEach((e) => e.remove());
  d.querySelectorAll("*").forEach((el) => {
    for (const a of Array.from(el.attributes)) {
      if (/^on/i.test(a.name) || /^\s*javascript:/i.test(a.value)) el.removeAttribute(a.name);
    }
  });
  // An image with nothing to load from keeps its place as its alt text.
  d.querySelectorAll("img").forEach((img) => {
    const src = img.getAttribute("src") || "";
    if (resolvable(src)) return;
    const span = d.createElement("span");
    span.setAttribute("data-unresolved", src);
    span.title = `${src} (not available here)`;
    span.textContent = `[${img.getAttribute("alt") || src || "image"}]`;
    img.replaceWith(span);
  });
  d.querySelectorAll("[src], [poster]").forEach((el) => {
    for (const a of ["src", "poster"]) {
      const v = el.getAttribute(a);
      if (v !== null && !resolvable(v)) el.removeAttribute(a);
    }
  });
  d.querySelectorAll("[srcset]").forEach((el) => el.removeAttribute("srcset"));
  d.querySelectorAll("a[href], area[href], link[href]").forEach((el) => {
    const href = el.getAttribute("href")!;
    if (!resolvable(href)) {
      el.removeAttribute("href");
      el.setAttribute("title", `${href} (not available here)`);
    } else if (href.startsWith("#")) el.setAttribute("target", "_self");
  });
  const head = d.head;
  const meta = d.createElement("meta");
  meta.httpEquiv = "Content-Security-Policy";
  meta.content = CSP;
  const target = d.createElement("base");
  target.target = "_blank";
  head.prepend(meta, target);
  return "<!doctype html>" + d.documentElement.outerHTML;
}
