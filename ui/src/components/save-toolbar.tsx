// ---
// relationships:
//   implements: web-ui
// ---
import type { ReactNode } from "react";

/** Keep form actions in normal flow until the editor scrolls past the viewport top. */
export function SaveToolbar({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div
      role="region"
      aria-label={label}
      className="bg-card text-card-foreground sticky top-0 z-20 flex flex-wrap items-center gap-3 rounded-lg border px-4 py-3 shadow-sm"
    >
      {children}
    </div>
  );
}
