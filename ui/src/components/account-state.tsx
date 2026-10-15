import { Badge } from "@/components/ui/badge";
import { Progress } from "@/components/ui/progress";
import type { Account, UsageWindow } from "@/lib/api";
import { relative } from "@/lib/format";

export { accountState, accountPath } from "@/lib/account-state";
import { accountState } from "@/lib/account-state";

export function AccountStateBadge({ account }: { account: Account }) {
  const s = accountState(account);
  return (
    <div className="flex flex-col gap-1">
      <Badge variant={s.variant}>{s.label}</Badge>
      {account.cooldown_until && account.cooldown_until > Date.now() / 1000 && (
        <span className="text-muted-foreground text-xs">
          {account.cooldown_reason || "Cooldown"} · {relative(account.cooldown_until)}
        </span>
      )}
    </div>
  );
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
  return <span className="font-mono text-xs">{windows.map((w) => `${w.name} ${Math.round(w.used_percent)}%`).join(" · ")}</span>;
}

export function WindowBars({ windows, softLimit }: { windows: UsageWindow[]; softLimit?: number }) {
  return (
    <div className="flex flex-col gap-2">
      {windows.map((w) => (
        <div key={w.name} className="flex flex-col gap-1">
          <div className="flex justify-between text-xs">
            <span>
              {w.name} window
              {w.resets_at ? <span className="text-muted-foreground"> · resets {relative(w.resets_at)}</span> : null}
            </span>
            <span className="font-mono">{Math.round(w.used_percent)}%</span>
          </div>
          <Progress
            value={w.used_percent}
            aria-label={`${w.name} window used`}
            indicatorClassName={softLimit !== undefined && w.used_percent >= softLimit ? "bg-usage-high" : undefined}
          />
        </div>
      ))}
    </div>
  );
}
