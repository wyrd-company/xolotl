// ---
// relationships:
//   implements: ../../../docs/user/web-ui.md
// ---
import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router";

import { CatalogRefresh } from "@/components/catalog-refresh";
import { Confirm } from "@/components/confirm";
import { Empty, Mono } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { del, post, type AccountKindInfo, type CatalogRefreshResult, type ProviderAccount } from "@/lib/api";
import { useAction } from "@/lib/hooks";
import { KeyState } from "@/pages/overview";

/** What changes when a provider account changes. */
export const PROVIDER_QUERIES = [["providers"], ["status"], ["catalog"], ["catalog-routes"], ["catalog-status"], ["policy-view"]];

export function kindLabel(kinds: AccountKindInfo[] | undefined, kind: string) {
  return kinds?.find((k) => k.kind === kind)?.label ?? kind;
}

/** Provider accounts: each one is a provider of its own, addressed by its name. */
export function ProviderAccounts({ providers, kinds, loaded }: { providers: ProviderAccount[]; kinds?: AccountKindInfo[]; loaded: boolean }) {
  return (
    <Card className="gap-0 pb-0">
      <CardHeader className="pb-4">
        <CardTitle>Provider accounts</CardTitle>
        <CardDescription>
          Upstreams that clients address by name: <Mono>name/model</Mono> and <Mono>/name/…</Mono>.
        </CardDescription>
      </CardHeader>
      <Table className="min-w-[760px]">
        <TableHeader>
          <TableRow>
            <TableHead>Provider</TableHead>
            <TableHead>Key</TableHead>
            <TableHead>Base URL</TableHead>
            <TableHead>Defined in</TableHead>
            <TableHead>
              <span className="sr-only">Actions</span>
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {providers.map((p) => (
            <ProviderRow key={p.name} p={p} kind={kindLabel(kinds, p.kind)} />
          ))}
        </TableBody>
      </Table>
      {loaded && providers.length === 0 && (
        <Empty>
          No provider accounts yet. <Link to="/accounts/add" className="underline">Add a provider account</Link>.
        </Empty>
      )}
    </Card>
  );
}

function ProviderRow({ p, kind }: { p: ProviderAccount; kind: string }) {
  const remove = useAction(() => del(`/admin/providers/${p.name}`), {
    invalidate: PROVIDER_QUERIES,
    success: `Removed ${p.name}`,
  });
  return (
    <TableRow>
      <TableCell>
        <Mono className="font-medium">{p.name}</Mono>
        {!p.enabled && (
          <Badge variant="secondary" className="ml-2">
            disabled
          </Badge>
        )}
        <div className="text-muted-foreground text-xs">{kind}</div>
      </TableCell>
      <TableCell>
        <KeyState provider={p} />
      </TableCell>
      <TableCell>
        <Mono className="text-muted-foreground text-xs break-all">{p.base_url ?? "default"}</Mono>
      </TableCell>
      <TableCell className="text-sm">{p.source === "config" ? "Config file" : "Added here"}</TableCell>
      <TableCell>
        <div className="flex justify-end gap-2">
          <ReplaceKey p={p} />
          {p.source === "vault" && (
            <Confirm
              title={`Remove ${p.name}?`}
              description="Its key is deleted from OpenBao, and clients can no longer reach its models. A policy that names it must be changed first."
              action="Remove"
              onConfirm={() => remove.mutate(undefined)}
            >
              <Button variant="ghost" size="sm" disabled={remove.isPending}>
                Remove
              </Button>
            </Confirm>
          )}
        </div>
      </TableCell>
    </TableRow>
  );
}

function ReplaceKey({ p }: { p: ProviderAccount }) {
  const [key, setKey] = useState("");
  const qc = useQueryClient();
  const save = useMutation({
    networkMode: "always",
    mutationFn: () =>
      post<{ catalog_refresh: CatalogRefreshResult | null }>(`/admin/providers/${p.name}/key`, { api_key: key.trim() }),
    onSuccess: () => {
      setKey("");
      for (const queryKey of PROVIDER_QUERIES) void qc.invalidateQueries({ queryKey });
    },
  });
  const id = `key-${p.name}`;
  const action = p.has_key ? "Replace key" : "Set key";
  return (
    <Dialog onOpenChange={(open) => {
      if (open) return;
      setKey("");
      if (!save.isPending) save.reset();
    }}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" aria-label={`${action} for ${p.name}`}>
          {action}
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[90dvh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Save key for {p.name}</DialogTitle>
          <DialogDescription className="wrap-anywhere">
            OpenBao path <Mono>{p.secret}</Mono>.
          </DialogDescription>
        </DialogHeader>
        {save.isSuccess ? (
          <>
            <ProviderKeyOutcome provider={p.name} discovery={save.data.catalog_refresh} />
            <DialogClose asChild>
              <Button variant="outline" className="justify-self-start">Done</Button>
            </DialogClose>
          </>
        ) : (
          <form
            className="flex flex-col gap-3"
            onSubmit={(e) => {
              e.preventDefault();
              if (key.trim() && !save.isPending) save.mutate();
            }}
          >
            <Label htmlFor={id}>API key</Label>
            <Input id={id} type="password" autoComplete="off" className="font-mono" value={key} onChange={(e) => setKey(e.target.value)} disabled={save.isPending} />
            {save.error && (
              <div role="alert" className="text-destructive text-sm wrap-anywhere">
                <p className="font-medium">Key save failed</p>
                <p>{save.error.message}</p>
                {p.workload_identity && <p>Update the provider's workload identity configuration instead.</p>}
              </div>
            )}
            <Button type="submit" className="self-start" disabled={!key.trim() || save.isPending}>
              Save key
            </Button>
            <p role="status" className="text-muted-foreground text-sm empty:sr-only">
              {save.isPending ? "Saving key and checking model discovery…" : ""}
            </p>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}

export function ProviderKeyOutcome({ provider, discovery }: { provider: string; discovery: CatalogRefreshResult | null }) {
  return (
    <div className="flex flex-col gap-3 text-sm wrap-anywhere">
      <div role="status">
        <p className="font-medium">Key saved for {provider}</p>
        {discovery === null
          ? <p>No model discovery result for this provider.</p>
          : !discovery.error && <p>{discovery.models} {discovery.models === 1 ? "model" : "models"} found.</p>}
      </div>
      {discovery?.error && (
        <>
          <div role="alert" className="text-warning">
            <p className="font-medium">Model discovery failed</p>
            <p>{discovery.error}</p>
            <p>Any cached or static models remain available. Use Refresh models to retry discovery.</p>
          </div>
          <CatalogRefresh providers={[provider]} />
        </>
      )}
    </div>
  );
}
