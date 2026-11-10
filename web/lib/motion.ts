import { useEffect } from "react";
// Quiet motion helpers. Durations come from the shared tokens, so reduced
// motion (which zeroes them) also zeroes every JS timer derived from them.
export function motionMs(token: string): number {
  if (typeof window === "undefined") return 0;
  if (matchMedia("(prefers-reduced-motion: reduce)").matches) return 0;
  const value = getComputedStyle(document.documentElement)
    .getPropertyValue(token)
    .trim();
  const n = parseFloat(value);
  if (!Number.isFinite(n)) return 0;
  return value.endsWith("ms") ? n : n * 1000;
}
// Restored geometry (stored nav width, theme, Back/Forward) paints without
// animation; user-initiated layout transitions start only after hydration.
export function useMotionReady() {
  useEffect(() => {
    const root = document.documentElement;
    const frame = requestAnimationFrame(() => (root.dataset.motion = "ready"));
    return () => {
      cancelAnimationFrame(frame);
      delete root.dataset.motion;
    };
  }, []);
}
