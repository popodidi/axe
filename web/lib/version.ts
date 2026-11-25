import type { ServerVersion } from "./api";

/** Where the server's commit links: its page in the axe repository. */
export const COMMIT_BASE = "https://github.com/popodidi/axe/commit/";

/**
 * The footer's build line from `GET /api/version`: `axe --version`'s string
 * split around its short commit, so the commit can link to GitHub. `null`
 * when the body is not a version (an older server answers 404).
 */
export function buildLine(
  v: unknown,
): { release: string; short: string | null; href: string | null } | null {
  if (!v || typeof v !== "object") return null;
  const { version, commit } = v as Partial<ServerVersion>;
  if (typeof version !== "string" || !version) return null;
  const m = version.match(/^(.*) \(([0-9a-f]+)\)$/);
  const full =
    typeof commit === "string" && /^[0-9a-f]{7,40}$/.test(commit) ? commit : null;
  if (!m) return { release: version, short: null, href: null };
  const [, release, short] = m;
  return {
    release,
    short,
    href: full?.startsWith(short) ? COMMIT_BASE + full : null,
  };
}
