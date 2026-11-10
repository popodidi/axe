export type LogLine = {
  start: number;
  end: number;
  text: string;
  fragment: boolean;
};
export type LogPage = {
  generation: string;
  start: number;
  end: number;
  size: number;
  lines: LogLine[];
};
export type LogBlock = { label: string; body: string; error?: boolean };
// Unknown records stay inspectable even when a future protocol is introduced.
export function formatLog(line: LogLine): LogBlock[] {
  if (line.fragment)
    return [
      {
        label: "Oversized record fragment · formatting unavailable",
        body: line.text,
      },
    ];
  try {
    const value = JSON.parse(line.text);
    if (value && typeof value === "object")
      return [
        {
          label: String(value.item?.type ?? value.type ?? "Record"),
          body: JSON.stringify(value, null, 2),
          error:
            !!value.error ||
            !!value.is_error ||
            ["error", "turn.failed"].includes(value.type),
        },
      ];
  } catch {
    /* Plain-text agent output is also valid. */
  }
  return [{ label: "Output", body: line.text }];
}

// Stop following at the cap instead of evicting unread scrollback. Older pages
// can replace the window; Jump to latest explicitly starts a fresh tail window.
export const MAX_LOG_BYTES = 2 * 1024 * 1024;
export const MAX_LOG_LINES = 4000;
export function logSize(lines: LogLine[]) {
  return lines.reduce((n, l) => n + l.text.length * 2, 0);
}
