// ---
// relationships:
//   implements: ../../../docs/user/web-ui.md
// ---
import { useState } from "react";

import { Confirm } from "@/components/confirm";
import { Empty, Mono } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { del, post, type AccountKindInfo, type ProviderAccount } from "@/lib/api";
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
          API-key upstreams. Clients address each one by its name: <Mono>name/model</Mono> and <Mono>/name/…</Mono>.
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
      {loaded && providers.length === 0 && <Empty>No provider accounts yet.</Empty>}
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
          {!p.no_auth && !p.workload_identity && p.enabled && <ReplaceKey p={p} />}
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
  const [open, setOpen] = useState(false);
  const [key, setKey] = useState("");
  const save = useAction(() => post(`/admin/providers/${p.name}/key`, { api_key: key.trim() }), {
    invalidate: PROVIDER_QUERIES,
    success: `Saved the ${p.name} key`,
    onSuccess: () => {
      setKey("");
      setOpen(false);
    },
  });
  const id = `key-${p.name}`;
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          {p.has_key ? "Replace key" : "Set key"}
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{p.has_key ? "Replace" : "Set"} the key of {p.name}</DialogTitle>
          <DialogDescription>
            Stored in OpenBao at <Mono>{p.secret}</Mono>.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-3"
          onSubmit={(e) => {
            e.preventDefault();
            save.mutate(undefined);
          }}
        >
          <Label htmlFor={id}>API key</Label>
          <Input id={id} type="password" autoComplete="off" className="font-mono" value={key} onChange={(e) => setKey(e.target.value)} />
          <Button type="submit" className="self-start" disabled={!key.trim() || save.isPending}>
            Save key
          </Button>
        </form>
      </DialogContent>
    </Dialog>
  );
}
