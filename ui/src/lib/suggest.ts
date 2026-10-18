// The suggestion list of a free-text combobox.

/** Up to `limit` options containing the typed text, without the value itself. */
export function suggestions(options: string[], value: string, limit = 8): string[] {
  const q = value.trim().toLowerCase();
  return options.filter((o) => o !== value && (!q || o.toLowerCase().includes(q))).slice(0, limit);
}

/** The highlighted index kept within the list; -1 when it is empty. */
export const highlighted = (active: number, shown: string[]) => Math.min(Math.max(active, 0), shown.length - 1);
