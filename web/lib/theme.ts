import { useCallback, useSyncExternalStore } from "react";
import { themeKey } from "./theme-script";
const event = "harvis-theme";
function subscribe(fn: () => void) {
  window.addEventListener(event, fn);
  return () => window.removeEventListener(event, fn);
}
const isLight = () => document.documentElement.dataset.theme === "light";
export function useTheme(): [boolean, () => void] {
  const light = useSyncExternalStore(subscribe, isLight, () => false);
  const toggle = useCallback(() => {
    const next = !isLight();
    const root = document.documentElement;
    // Quiet: the palette changes immediately; suspend hover/color transitions
    // for one frame so nothing crossfades between themes.
    root.dataset.themeSwitching = "";
    if (next) root.dataset.theme = "light";
    else delete root.dataset.theme;
    requestAnimationFrame(() =>
      requestAnimationFrame(() => delete root.dataset.themeSwitching),
    );
    try {
      localStorage.setItem(themeKey, next ? "light" : "dark");
    } catch {
      // Storage may be unavailable (private mode); the switch still applies.
    }
    window.dispatchEvent(new Event(event));
  }, []);
  return [light, toggle];
}
