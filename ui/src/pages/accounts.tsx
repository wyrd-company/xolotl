import { useState } from "react";
import { toast } from "sonner";
import { AccountLogin } from "@/pages/add-account";
import { useQuery } from "@tanstack/react-query";
import { Link, useSearchParams } from "react-router";

import { CatalogRefresh } from "@/components/catalog-refresh";
import { accountName, AccountStateBadge, WindowBars, WindowsSummary } from "@/components/account-state";
import { Confirm } from "@/components/confirm";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { del, get, post, PROVIDER_NAMES, SUB_PROVIDERS, type Account, type Status } from "@/lib/api";
import { dateTime, relative } from "@/lib/format";
import { useAction } from "@/lib/hooks";

export function AccountsPage() {
  const [search, setSearch] = useSearchParams();
  const [filter, setFilter] = useState("all");
  const accounts = useQuery({
    queryKey: ["accounts"],
    queryFn: () => get<Account[]>("/admin/accounts"),
    refetchInterval: 30_000,
  });
  const status = useQuery({
    queryKey: ["status"],
    queryFn: () => get<Status>("/admin/status"),
  });
  const all = accounts.data ?? [];
  const shown = filter === "all" ? all : all.filter((a) => a.provider === filter);
  const sub = status.data?.subscription;

  return (
    <>
      <PageHeader
        title="Subscription accounts"
        description={
          sub && (
            <>
              Strategy: {sub.strategy} · session affinity {Math.round(sub.session_affinity_secs / 60)} min · accounts above{" "}
              {sub.soft_limit_percent}% are used last
            </>
          )
        }
      >
        <Button asChild>
          <Link to="/accounts/add">Add account</Link>
        </Button>
      </PageHeader>
      <QueryError error={accounts.error} />

      <Tabs value={filter} onValueChange={setFilter}>
        <TabsList aria-label="Provider">
          <TabsTrigger value="all">All ({all.length})</TabsTrigger>
          {SUB_PROVIDERS.map((p) => (
            <TabsTrigger key={p} value={p}>
              {PROVIDER_NAMES[p]} ({all.filter((a) => a.provider === p).length})
            </TabsTrigger>
          ))}
        </TabsList>
      </Tabs>

      <CatalogRefresh providers={filter === "all" ? SUB_PROVIDERS : [filter]} />

      <Card className="py-0">
        <Table className="min-w-[960px]">
          <TableHeader>
            <TableRow>
              <TableHead>Account</TableHead>
              <TableHead>Plan</TableHead>
              <TableHead>State</TableHead>
              <TableHead>Usage</TableHead>
              <TableHead>Priority</TableHead>
              <TableHead>Last used</TableHead>
              <TableHead>
                <span className="sr-only">Actions</span>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {shown.map((a) => (
              <AccountRow
                key={a.id}
                account={a}
                open={search.get("account") === a.id}
                onOpenChange={(open) => setSearch(open ? { account: a.id } : {})}
              />
            ))}
          </TableBody>
        </Table>
        {accounts.isSuccess && shown.length === 0 && (
          <Empty>
            No accounts yet. <Link to="/accounts/add">Add one</Link>.
          </Empty>
        )}
      </Card>
    </>
  );
}

function AccountRow({ account: a, open, onOpenChange }: { account: Account; open: boolean; onOpenChange: (open: boolean) => void }) {
  const inv = {
    invalidate: [["accounts"], ["status"], ["catalog"], ["catalog-routes"], ["policy-view"]],
  };
  const refresh = useAction(() => post(`/admin/accounts/${a.id}/usage`), {
    ...inv,
    success: "Usage refreshed",
  });
  const toggle = useAction(() => post(`/admin/accounts/${a.id}/${a.enabled ? "disable" : "enable"}`), {
    ...inv,
    success: a.enabled ? "Account disabled" : "Account enabled",
  });

  return (
    <TableRow>
      <TableCell>
        <div className="font-medium">{accountName(a)}</div>
        <Mono className="text-muted-foreground text-xs">
          {a.provider} · {a.id}
        </Mono>
      </TableCell>
      <TableCell>{a.plan ?? "-"}</TableCell>
      <TableCell>
        <AccountStateBadge account={a} />
      </TableCell>
      <TableCell>
        <WindowsSummary windows={a.usage?.windows} />
      </TableCell>
      <TableCell className="font-mono">{a.priority}</TableCell>
      <TableCell className="whitespace-nowrap">{relative(a.last_used_at)}</TableCell>
      <TableCell>
        <div className="flex justify-end gap-2">
          <Button size="sm" variant="outline" disabled={refresh.isPending || !a.enabled} onClick={() => refresh.mutate(undefined)}>
            Refresh usage
          </Button>
          <RefreshModels account={a} />
          <Button size="sm" variant="outline" disabled={toggle.isPending} onClick={() => toggle.mutate(undefined)}>
            {a.enabled ? "Disable" : "Enable"}
          </Button>
          <AccountDetails account={a} open={open} onOpenChange={onOpenChange} />
        </div>
      </TableCell>
    </TableRow>
  );
}

function AccountDetails({
  account: a,
  open,
  onOpenChange: setOpen,
}: {
  account: Account;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [priority, setPriority] = useState(String(a.priority));
  const inv = {
    invalidate: [["accounts"], ["status"], ["catalog"], ["catalog-routes"], ["policy-view"]],
  };
  const setPrio = useAction((p: number) => post(`/admin/accounts/${a.id}/priority`, { priority: p }), {
    ...inv,
    success: "Priority saved",
  });
  const reset = useAction(() => post<{ catalog_refresh: ModelRefresh }>(`/admin/accounts/${a.id}/reset`), {
    ...inv,
    success: "Cooldown and auth failure cleared",
    onSuccess: (r) => showModels(r.catalog_refresh),
  });
  const remove = useAction(() => del(`/admin/accounts/${a.id}`), {
    ...inv,
    success: "Account removed",
    onSuccess: () => setOpen(false),
  });
  const coolingOrFailed = a.auth_failed || (a.cooldown_until ?? 0) > Date.now() / 1000;

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (o) setPriority(String(a.priority));
      }}
    >
      <DialogTrigger asChild>
        <Button size="sm" variant="ghost" aria-label={`Details for ${accountName(a)}`}>
          Details
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{accountName(a)}</DialogTitle>
          <DialogDescription>
            <Mono>
              {a.provider} · {a.id}
            </Mono>
          </DialogDescription>
        </DialogHeader>
        <CatalogRefresh providers={[a.provider]} />
        <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2 text-sm">
          <dt className="text-muted-foreground">State</dt>
          <dd>
            <AccountStateBadge account={a} />
          </dd>
          {a.label && a.label !== a.email && (
            <>
              <dt className="text-muted-foreground">Label</dt>
              <dd>{a.label}</dd>
            </>
          )}
          <dt className="text-muted-foreground">Plan</dt>
          <dd>{a.plan ?? "-"}</dd>
          {a.provider === "codex" && (
            <>
              <dt className="text-muted-foreground">Reset credits</dt>
              <dd>{a.usage?.raw?.reset_credits ?? "Unknown"}</dd>
            </>
          )}
          {a.cooldown_reason && (
            <>
              <dt className="text-muted-foreground">Cooldown reason</dt>
              <dd>{a.cooldown_reason}</dd>
            </>
          )}
          {a.last_error && (
            <>
              <dt className="text-muted-foreground">Last error</dt>
              <dd className="break-words">{a.last_error}</dd>
            </>
          )}
          {a.expires_at != null && (
            <>
              <dt className="text-muted-foreground">Token expires</dt>
              <dd>{relative(a.expires_at)}</dd>
            </>
          )}
          {a.unsaved && (
            <>
              <dt className="text-muted-foreground">Vault</dt>
              <dd className="text-warning">Refreshed token not saved yet; retrying</dd>
            </>
          )}
          <dt className="text-muted-foreground">Added</dt>
          <dd>{dateTime(a.created_at)}</dd>
          <dt className="text-muted-foreground">Hub index</dt>
          <dd>
            <Mono>{a.auth_index}</Mono>
          </dd>
        </dl>
        {a.usage?.windows.length ? (
          <div className="flex flex-col gap-2">
            <div className="text-sm font-medium">Usage · fetched {relative(a.usage.fetched_at)}</div>
            <WindowBars windows={a.usage.windows} />
          </div>
        ) : null}
        <form
          className="flex items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            setPrio.mutate(Number(priority));
          }}
        >
          <div className="flex flex-col gap-2">
            <Label htmlFor={`prio-${a.id}`}>Priority (higher is used first)</Label>
            <Input
              id={`prio-${a.id}`}
              type="number"
              step={1}
              className="w-32"
              value={priority}
              onChange={(e) => setPriority(e.target.value)}
            />
          </div>
          <Button type="submit" variant="outline" disabled={setPrio.isPending || priority === "" || Number(priority) === a.priority}>
            Save
          </Button>
        </form>
        <p className="text-muted-foreground text-sm">
          Retry clears the auth failure and cooldown so this account can be tried again. It does not change the credential or reset upstream
          quota.
        </p>
        {a.provider === "opencode-go" ? <ReplaceKey account={a} /> : <AccountLogin provider={a.provider} />}
        <div className="flex flex-wrap gap-2 border-t pt-4">
          <Button variant="outline" disabled={!coolingOrFailed || reset.isPending} onClick={() => reset.mutate(undefined)}>
            Retry
          </Button>
          <Confirm
            title="Remove this account?"
            description="Its credentials are deleted from OpenBao. Add the account again to restore it."
            action="Remove"
            onConfirm={() => remove.mutate(undefined)}
          >
            <Button variant="destructive" disabled={remove.isPending}>
              Remove
            </Button>
          </Confirm>
        </div>
      </DialogContent>
    </Dialog>
  );
}

type ModelRefresh = { provider: string; models?: number; error?: string };
function showModels(r: ModelRefresh) {
  if (r.error) toast.error(`${r.provider}: ${r.error}`);
  else toast.success(`${r.provider}: ${r.models} models found`);
}

function RefreshModels({ account: a }: { account: Account }) {
  const refresh = useAction(() => post<ModelRefresh>(`/admin/accounts/${a.id}/models`), {
    invalidate: [["catalog"], ["catalog-routes"], ["policy-view"], ["status"]],
    onSuccess: showModels,
  });
  return (
    <Button size="sm" variant="outline" disabled={refresh.isPending} onClick={() => refresh.mutate(undefined)}>
      Refresh models
    </Button>
  );
}

function ReplaceKey({ account: a }: { account: Account }) {
  const [key, setKey] = useState("");
  const replace = useAction(
    () =>
      post<{ catalog_refresh: ModelRefresh }>(`/admin/accounts/${a.id}/key`, {
        api_key: key.trim(),
      }),
    {
      invalidate: [["accounts"], ["status"], ["catalog"], ["catalog-routes"], ["policy-view"]],
      success: "Key replaced",
      onSuccess: (r) => {
        setKey("");
        showModels(r.catalog_refresh);
      },
    },
  );
  return (
    <form
      className="flex flex-col gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        replace.mutate(undefined);
      }}
    >
      <Label htmlFor={`key-${a.id}`}>Replace key</Label>
      <Input id={`key-${a.id}`} type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)} />
      <Button type="submit" className="self-start" disabled={!key.trim() || replace.isPending}>
        Replace key
      </Button>
    </form>
  );
}
