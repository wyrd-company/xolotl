import { accountName, accountState } from "@/components/account-state";
import { Mono } from "@/components/layout";
import { Section, SectionContent } from "@/components/section";
import { Badge } from "@/components/ui/badge";
import { Checkbox } from "@/components/ui/checkbox";
import { SUB_PROVIDERS, type Account } from "@/lib/api";
import { usable, type Health } from "@/lib/policy-models";
import { cn } from "@/lib/utils";

import type { Draft, SetDraft } from "./policies";
import { HealthBadge } from "./policy-model-list";

const isPool = (p: string) => (SUB_PROVIDERS as string[]).includes(p);

/**
 * The providers keys may use and, for each subscription provider, the
 * accounts of its pool. A new provider starts with the accounts it has now;
 * `*` also takes accounts added later.
 */
export function ProviderAccess({
  draft,
  set,
  known,
  accounts,
  health,
  ready,
}: {
  draft: Draft;
  set: SetDraft;
  known: string[];
  accounts: Account[];
  health: (provider: string) => Health;
  ready: boolean;
}) {
  const all = draft.providers.includes("*");
  const allowed = (p: string) => all || draft.providers.includes(p);
  const pool = (p: string) => accounts.filter((a) => a.provider === p);
  /** A new entry: the pool's accounts now. */
  const fresh = (p: string) => pool(p).map((a) => a.id);

  function setAccounts(next: Record<string, string[]>) {
    set("accounts", next);
  }
  function toggleProvider(p: string, on: boolean) {
    set("providers", on ? [...draft.providers, p] : draft.providers.filter((x) => x !== p));
    if (!isPool(p)) return;
    const next = { ...draft.accounts };
    if (on && fresh(p).length) next[p] = fresh(p);
    else delete next[p];
    setAccounts(next);
  }
  function toggleAll(on: boolean) {
    set("providers", on ? ["*"] : []);
    const next: Record<string, string[]> = {};
    if (on) for (const p of known.filter(isPool)) if (draft.accounts[p] ?? fresh(p).length) next[p] = draft.accounts[p] ?? fresh(p);
    setAccounts(next);
  }
  function toggleAccount(p: string, id: string, on: boolean) {
    const ids = draft.accounts[p] ?? [];
    const explicit = ids.includes("*") ? pool(p).map((a) => a.id) : ids;
    const next = { ...draft.accounts, [p]: on ? [...explicit, id] : explicit.filter((x) => x !== id) };
    if (next[p].length === 0) delete next[p];
    setAccounts(next);
  }
  function toggleEvery(p: string, on: boolean) {
    const next = { ...draft.accounts, [p]: on ? ["*"] : fresh(p) };
    if (next[p].length === 0) delete next[p];
    setAccounts(next);
  }

  const count = all ? "all providers" : `${draft.providers.length} provider${draft.providers.length === 1 ? "" : "s"}`;

  return (
    <Section
      id="policy-providers"
      title="Providers and accounts"
      summary={count}
      description={
        <>
          The providers keys under this policy may use. A subscription provider serves them only from the accounts checked here; with none
          checked it serves nothing. <Mono>*</Mono> also takes accounts added later.
        </>
      }
    >
      <SectionContent className="flex flex-col gap-3">
        <label className="flex items-center gap-2 text-sm">
          <Checkbox checked={all} onCheckedChange={(c) => toggleAll(c === true)} />
          all providers (<span className="font-mono">*</span>)
        </label>
        <ul className="flex flex-col divide-y rounded-md border">
          {known.map((p) => {
            const on = allowed(p);
            const ids = draft.accounts[p] ?? [];
            const every = ids.includes("*");
            const mine = pool(p);
            const missing = every ? [] : ids.filter((id) => !mine.some((a) => a.id === id));
            const checked = (id: string) => every || ids.includes(id);
            return (
              <li key={p} className="flex flex-col gap-2 px-3 py-2.5">
                <label className={cn("flex items-center gap-2 text-sm", ready && !usable(health(p)) && on && "text-muted-foreground")}>
                  <Checkbox checked={on} disabled={all} onCheckedChange={(c) => toggleProvider(p, c === true)} />
                  <span className="font-mono">{p}</span>
                  {ready && on && <HealthBadge health={health(p)} />}
                  {!isPool(p) && <span className="text-muted-foreground text-xs">gateway</span>}
                </label>
                {on && isPool(p) && (
                  <div className="flex flex-col gap-1.5 pl-6">
                    {mine.map((a) => {
                      const s = accountState(a);
                      return (
                        <label key={a.id} className={cn("flex items-center gap-2 text-sm", !checked(a.id) && "text-muted-foreground")}>
                          <Checkbox
                            checked={checked(a.id)}
                            disabled={every}
                            aria-label={`Use ${accountName(a)}`}
                            onCheckedChange={(c) => toggleAccount(p, a.id, c === true)}
                          />
                          <span>{accountName(a)}</span>
                          <span className="text-muted-foreground font-mono text-xs">{a.id}</span>
                          <Badge variant={s.variant}>{s.label}</Badge>
                        </label>
                      );
                    })}
                    {missing.map((id) => (
                      <label key={id} className="text-muted-foreground flex items-center gap-2 text-sm">
                        <Checkbox checked aria-label={`Use ${id}`} onCheckedChange={() => toggleAccount(p, id, false)} />
                        <span className="font-mono text-xs">{id}</span>
                        <Badge variant="warning">no such account</Badge>
                      </label>
                    ))}
                    {ready && mine.length === 0 && <span className="text-muted-foreground text-sm">No accounts yet.</span>}
                    <label className="flex items-center gap-2 text-sm">
                      <Checkbox checked={every} onCheckedChange={(c) => toggleEvery(p, c === true)} />
                      every account, also new ones (<span className="font-mono">*</span>)
                    </label>
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      </SectionContent>
    </Section>
  );
}
