import { Badge } from "@/components/ui/badge";
import { Progress } from "@/components/ui/progress";
import type { Account, UsageWindow } from "@/lib/api";
import { clock, relative } from "@/lib/format";

export function accountState(a: Account): { label: string; variant: "success" | "destructive" | "info" | "secondary" | "warning" } {
  if (!a.enabled) return { label: "Disabled", variant: "secondary" };
  if (a.auth_failed) return { label: "Re-login needed", variant: "destructive" };
  if (a.cooldown_until && a.cooldown_until > Date.now() / 1000) return { label: `Cooling until ${clock(a.cooldown_until)}`, variant: "info" };
  if (a.available) return { label: "Available", variant: "success" };
  return { label: "Unavailable", variant: "warning" };
}

export function AccountStateBadge({ account }: { account: Account }) {
  const s = accountState(account);
  return <Badge variant={s.variant}>{s.label}</Badge>;
}

export function accountName(a: Account): string {
  return a.email || a.label || a.id;
}

/** Highest usage across an account's windows (0 when unknown). */
export function peakUsage(a: Account): number {
  return Math.max(0, ...(a.usage?.windows.map((w) => w.used_percent) ?? []));
}

export function WindowsSummary({ windows }: { windows: UsageWindow[] | undefined }) {
  if (!windows?.length) return <span className="text-muted-foreground">-</span>;
  return (
    <span className="font-mono text-xs">
      {windows.map((w) => `${w.name} ${Math.round(w.used_percent)}%`).join(" · ")}
    </span>
  );
}

export function WindowBars({ windows, softLimit }: { windows: UsageWindow[]; softLimit?: number }) {
  return (
    <div className="flex flex-col gap-2">
      {windows.map((w) => (
        <div key={w.name} className="flex flex-col gap-1">
          <div className="flex justify-between text-xs">
            <span>
              {w.name} window{w.resets_at ? <span className="text-muted-foreground"> · resets {relative(w.resets_at)}</span> : null}
            </span>
            <span className="font-mono">{Math.round(w.used_percent)}%</span>
          </div>
          <Progress
            value={w.used_percent}
            aria-label={`${w.name} window used`}
            indicatorClassName={softLimit !== undefined && w.used_percent >= softLimit ? "bg-[#b45309]" : undefined}
          />
        </div>
      ))}
    </div>
  );
}
