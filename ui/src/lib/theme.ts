import { useSyncExternalStore } from "react";

/** `system` follows the browser's `prefers-color-scheme`. */
export type Theme = "system" | "light" | "dark";

const KEY = "xolotl-theme";
const listeners = new Set<() => void>();

function stored(): Theme {
  try {
    const t = localStorage.getItem(KEY);
    return t === "light" || t === "dark" ? t : "system";
  } catch {
    return "system";
  }
}

let current: Theme = stored();

/** Sets `data-theme` on <html>; the stylesheet picks the palette from it. */
export function applyTheme(theme: Theme = current) {
  const root = document.documentElement;
  if (theme === "system") delete root.dataset.theme;
  else root.dataset.theme = theme;
}

export function setTheme(theme: Theme) {
  current = theme;
  try {
    if (theme === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, theme);
  } catch {
    // Storage blocked: the choice lasts for this page only.
  }
  applyTheme(theme);
  listeners.forEach((l) => l());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useTheme(): Theme {
  return useSyncExternalStore(subscribe, () => current);
}
