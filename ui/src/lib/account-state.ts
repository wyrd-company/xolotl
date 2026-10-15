// relationships:
//   references: docs/subscriptions.md
import type { Account } from "./api";

export function accountPath(id: string): string {
  return `/accounts?account=${encodeURIComponent(id)}`;
}

export function accountState(a: Account): {
  label: string;
  variant: "success" | "destructive" | "info" | "secondary" | "warning";
} {
  if (!a.enabled) return { label: "Disabled", variant: "secondary" };
  if (a.auth_failed)
    return {
      label: a.provider !== "opencode-go" ? "Re-login needed" : /\b4(?!01)\d{2}\b/.test(a.last_error ?? "") ? "Refused" : "Key rejected",
      variant: "destructive",
    };
  if (a.cooldown_until && a.cooldown_until > Date.now() / 1000) return { label: "Cooling down", variant: "info" };
  if (a.available) return { label: "Available", variant: "success" };
  return { label: "Unavailable", variant: "warning" };
}
