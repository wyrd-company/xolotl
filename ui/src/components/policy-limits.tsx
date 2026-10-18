import type { ReactNode } from "react";
import { Plus, Trash2 } from "lucide-react";

import { accountName } from "@/components/account-state";
import { Mono } from "@/components/layout";
import { Section, SectionContent } from "@/components/section";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import type { Account, Allowance, Consumption, Limits, PolicyView } from "@/lib/api";
import { num } from "@/lib/format";

const FIELDS = [
  ["requests_per_minute", "Requests per minute"],
  ["requests_per_day", "Requests per day"],
  ["tokens_per_minute", "Tokens per minute"],
  ["tokens_per_day", "Tokens per day"],
] as const;

type Field = (typeof FIELDS)[number][0];

function withField<T extends Allowance>(a: T, field: Field, raw: string): T {
  const next = { ...a };
  const n = Number(raw);
  if (raw.trim() === "" || !Number.isFinite(n)) delete next[field];
  else next[field] = Math.max(0, Math.floor(n));
  return next;
}

const setCount = (n: number, what: string) => `${n} ${what}${n === 1 ? "" : "s"}`;

/**
 * Rate limits and token budgets of a policy: of every request, and of the
 * requests to one provider or one account. `view` carries the policy's
 * current consumption (all keys together).
 */
export function PolicyLimits({
  limits,
  onChange,
  view,
  providers,
  accounts,
}: {
  limits: Limits;
  onChange: (l: Limits) => void;
  view?: PolicyView;
  /** Providers the policy allows. */
  providers: string[];
  /** Accounts the policy uses. */
  accounts: Account[];
}) {
  const total = FIELDS.filter(([f]) => limits[f] !== undefined).length;
  const byProvider = limits.providers ?? {};
  const byAccount = limits.accounts ?? {};
  const summary =
    [
      total ? setCount(total, "limit") : "",
      Object.keys(byProvider).length ? setCount(Object.keys(byProvider).length, "provider") : "",
      Object.keys(byAccount).length ? setCount(Object.keys(byAccount).length, "account") : "",
    ]
      .filter(Boolean)
      .join(" · ") || "unlimited";
  const setMap = (k: "providers" | "accounts", m: Record<string, Allowance>) => {
    const next = { ...limits, [k]: m };
    if (Object.keys(m).length === 0) delete next[k];
    onChange(next);
  };
  const names = new Map(accounts.map((a) => [a.id, accountName(a)]));

  return (
    <Section
      id="policy-limits"
      title="Rate limits"
      summary={summary}
      description={
        <>
          Limits on the LLM endpoints over a sliding minute and day; empty is unlimited. Tokens are input plus output as the upstream reports them,
          counted when a response finishes. A request over a limit gets 429 with <code>retry-after</code>.
        </>
      }
    >
      <SectionContent className="flex flex-col gap-6">
        <div className="flex max-w-sm flex-col gap-2">
          <Label htmlFor="limits-per">Counted</Label>
          <Select value={limits.per ?? "key"} onValueChange={(v) => onChange({ ...limits, per: v === "policy" ? "policy" : undefined })}>
            <SelectTrigger id="limits-per">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="key">per key: each key has its own allowance</SelectItem>
              <SelectItem value="policy">per policy: all keys share one allowance</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div className="flex flex-col gap-3">
          <h3 className="text-sm font-semibold">Every request</h3>
          <AllowanceFields id="limits" allowance={limits} onChange={(a) => onChange(a)} usage={view?.usage} />
        </div>
        <ScopedLimits
          title="Per provider"
          description="Requests sent to one provider. An alias goes on to its next target when a provider is at its limit."
          entries={byProvider}
          options={providers.map((p) => ({ value: p, label: p }))}
          label={(p) => <Mono>{p}</Mono>}
          usage={view?.provider_usage}
          onChange={(m) => setMap("providers", m)}
        />
        <ScopedLimits
          title="Per account"
          description="Requests sent to one subscription account. An account at its limit is skipped; the pool serves from its other accounts."
          entries={byAccount}
          options={accounts.map((a) => ({ value: a.id, label: `${accountName(a)} (${a.provider})` }))}
          label={(id) => (
            <span className="flex flex-col">
              <span>{names.get(id) ?? id}</span>
              <span className="text-muted-foreground font-mono text-xs">{id}</span>
            </span>
          )}
          usage={view?.account_usage}
          onChange={(m) => setMap("accounts", m)}
        />
      </SectionContent>
    </Section>
  );
}

function AllowanceFields<T extends Allowance>({
  id,
  allowance,
  onChange,
  usage,
}: {
  id: string;
  allowance: T;
  onChange: (a: T) => void;
  usage?: Consumption;
}) {
  return (
    <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
      {FIELDS.map(([field, label]) => (
        <div key={field} className="flex flex-col gap-2">
          <Label htmlFor={`${id}-${field}`}>{label}</Label>
          <Input
            id={`${id}-${field}`}
            type="number"
            min={1}
            step={1}
            inputMode="numeric"
            className="font-mono"
            placeholder="unlimited"
            value={allowance[field] ?? ""}
            onChange={(e) => onChange(withField(allowance, field, e.target.value))}
          />
          {usage && <span className="text-muted-foreground text-xs">all keys now: {num(usage[field])}</span>}
        </div>
      ))}
    </div>
  );
}

function ScopedLimits({
  title,
  description,
  entries,
  options,
  label,
  usage,
  onChange,
}: {
  title: string;
  description: string;
  entries: Record<string, Allowance>;
  options: { value: string; label: string }[];
  label: (key: string) => ReactNode;
  usage?: Record<string, Consumption>;
  onChange: (m: Record<string, Allowance>) => void;
}) {
  const free = options.filter((o) => !(o.value in entries));
  return (
    <div className="flex flex-col gap-3 border-t pt-4">
      <div className="flex flex-col gap-1">
        <h3 className="text-sm font-semibold">{title}</h3>
        <p className="text-muted-foreground text-sm">{description}</p>
      </div>
      {Object.entries(entries).map(([key, a]) => (
        <div key={key} className="flex flex-col gap-2 rounded-md border p-3">
          <div className="flex items-center gap-2 text-sm">
            <span className="flex-1">{label(key)}</span>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              aria-label={`Remove the limits of ${key}`}
              onClick={() => onChange(Object.fromEntries(Object.entries(entries).filter(([k]) => k !== key)))}
            >
              <Trash2 />
            </Button>
          </div>
          <AllowanceFields id={`limits-${key}`} allowance={a} onChange={(next) => onChange({ ...entries, [key]: next })} usage={usage?.[key]} />
        </div>
      ))}
      {free.length > 0 ? (
        <Select value="" onValueChange={(v) => onChange({ ...entries, [v]: {} })}>
          <SelectTrigger aria-label={`Add a limit ${title.toLowerCase()}`} className="h-9 w-72">
            <span className="flex items-center gap-2">
              <Plus className="size-4" /> Add a limit
            </span>
          </SelectTrigger>
          <SelectContent>
            {free.map((o) => (
              <SelectItem key={o.value} value={o.value}>
                {o.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : (
        Object.keys(entries).length === 0 && <p className="text-muted-foreground text-sm">Nothing to limit.</p>
      )}
    </div>
  );
}
