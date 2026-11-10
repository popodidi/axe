import { safeLink } from "./model";
// Labels describe only the recorded URL; never infer external titles or status.
export function artifactReference(value: string) {
  const href = safeLink(value);
  if (!href)
    return {
      type: "Reference",
      name: value || "Empty reference",
      source: "—",
      href,
    };
  const url = new URL(href);
  const parts = url.pathname.split("/").filter(Boolean);
  const source = url.hostname;
  const repo = parts.slice(0, 2).join("/");
  if (
    source === "github.com" &&
    parts[2] === "pull" &&
    /^\d+$/.test(parts[3] || "")
  )
    return { type: "PR", name: `PR #${parts[3]} · ${repo}`, source, href };
  if (
    source === "github.com" &&
    parts[2] === "actions" &&
    parts[3] === "runs" &&
    /^\d+$/.test(parts[4] || "")
  )
    return { type: "CI", name: `CI run ${parts[4]} · ${repo}`, source, href };
  const record =
    url.searchParams.get("record") ||
    (parts[0] === "record" ? parts[1] : undefined);
  if (
    (source.endsWith(".larksuite.com") || source.endsWith(".feishu.cn")) &&
    record
  )
    return { type: "Lark", name: `Lark record ${record}`, source, href };
  let filename = parts.at(-1) || "";
  try {
    filename = decodeURIComponent(filename);
  } catch {
    /* Keep malformed encodings readable and lossless. */
  }
  if (/\.[a-z0-9]{1,10}$/i.test(filename))
    return { type: "File", name: filename, source, href };
  return { type: "Link", name: value, source, href };
}
