import { useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { Confirm } from "@/components/confirm";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { del, get, post, type KeyInfo, type Policy, type Status, type UsageRow } from "@/lib/api";
import { compact, num, relativeIso } from "@/lib/format";
import { useAction } from "@/lib/hooks";

const WEEK = 7 * 86_400;
const TTLS: [string, number | null][] = [
  ["Never", null],
  ["30 days", 30 * 86_400],
  ["90 days", 90 * 86_400],
  ["1 year", 365 * 86_400],
];
const GRACES: [string, number][] = [
  ["None: the old key stops now", 0],
  ["15 minutes", 900],
  ["1 hour", 3600],
  ["1 day", 86_400],
];

export function KeysPage() {
  const keys = useQuery({ queryKey: ["keys"], queryFn: () => get<KeyInfo[]>("/admin/keys") });
  const status = useQuery({ queryKey: ["status"], queryFn: () => get<Status>("/admin/status") });
  const usage = useQuery({
    queryKey: ["usage", "client", WEEK],
    queryFn: () => get<UsageRow[]>(`/admin/usage?group_by=client&since_secs=${WEEK}`),
  });
  const [issued, setIssued] = useState<KeyInfo | null>(null);
  const requests = new Map((usage.data ?? []).map((r) => [r.key, r.requests]));
  const fullPath = (k: KeyInfo) => (status.data ? `${status.data.vault_mount}/${status.data.vault_prefix}/${k.vault_path}` : k.vault_path);

  return (
    <>
      <PageHeader
        title="Client keys"
        description="Keys are written to OpenBao and never shown here. Each client reads its own key with a Vault policy scoped to its path."
      />
      <IssueKey onIssued={setIssued} />
      {issued && (
        <Card role="status" aria-label="Issued" className="border-success/30 bg-success-muted gap-3">
          <CardHeader>
            <CardTitle className="text-success text-base">Issued {issued.client_id}</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-2 text-sm">
            <div>The client reads it with:</div>
            <pre className="bg-card overflow-x-auto rounded-md border p-3 font-mono text-xs">
              bao kv get -field=api_key {fullPath(issued)}
            </pre>
            <div>Grant it this Vault policy:</div>
            <pre className="bg-card overflow-x-auto rounded-md border p-3 font-mono text-xs">
              {`path "${status.data?.vault_mount ?? "secret"}/data/${status.data?.vault_prefix ?? "xolotl"}/${issued.vault_path}" { capabilities = ["read"] }`}
            </pre>
            <Button variant="ghost" size="sm" className="self-start" onClick={() => setIssued(null)}>
              Dismiss
            </Button>
          </CardContent>
        </Card>
      )}
      <QueryError error={keys.error} />
      <Card className="py-0">
        <Table className="min-w-[860px]">
          <TableHeader>
            <TableRow>
              <TableHead>Client</TableHead>
              <TableHead>State</TableHead>
              <TableHead>Vault path</TableHead>
              <TableHead>Rotated</TableHead>
              <TableHead>Expires</TableHead>
              <TableHead className="text-right">Requests (7d)</TableHead>
              <TableHead>Rate limits</TableHead>
              <TableHead>
                <span className="sr-only">Actions</span>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(keys.data ?? []).map((k) => (
              <KeyRow key={k.client_id} k={k} requests={requests.get(k.client_id) ?? 0} />
            ))}
          </TableBody>
        </Table>
        {keys.isSuccess && keys.data.length === 0 && <Empty>No client keys yet.</Empty>}
      </Card>
    </>
  );
}

function IssueKey({ onIssued }: { onIssued: (k: KeyInfo) => void }) {
  const policies = useQuery({ queryKey: ["policies"], queryFn: () => get<Policy[]>("/admin/policies") });
  const [policy, setPolicy] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [ttl, setTtl] = useState("0");
  const issue = useAction(
    () => post<KeyInfo>("/admin/keys", { policy, name: name.trim(), description: description.trim(), ttl_secs: TTLS[Number(ttl)][1] }),
    {
      invalidate: [["keys"]],
      onSuccess: (k) => {
        onIssued(k);
        setName("");
        setDescription("");
      },
    },
  );
  const noPolicies = policies.isSuccess && policies.data.length === 0;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Issue a key</CardTitle>
      </CardHeader>
      <CardContent>
        {noPolicies ? (
          <p className="text-muted-foreground text-sm">Create a policy first: every key belongs to one.</p>
        ) : (
          <form
            className="flex flex-wrap items-end gap-4"
            onSubmit={(e) => {
              e.preventDefault();
              issue.mutate(undefined);
            }}
          >
            <div className="flex flex-col gap-2">
              <Label htmlFor="k-policy">Policy</Label>
              <Select value={policy} onValueChange={setPolicy}>
                <SelectTrigger id="k-policy" className="w-48">
                  <SelectValue placeholder="Choose…" />
                </SelectTrigger>
                <SelectContent>
                  {(policies.data ?? []).map((p) => (
                    <SelectItem key={p.name} value={p.name}>
                      {p.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="flex flex-col gap-2">
              <Label htmlFor="k-name">Client name</Label>
              <Input id="k-name" className="w-48" placeholder="laptop" value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <div className="flex min-w-48 flex-1 flex-col gap-2">
              <Label htmlFor="k-desc">Description</Label>
              <Input id="k-desc" value={description} onChange={(e) => setDescription(e.target.value)} />
            </div>
            <div className="flex flex-col gap-2">
              <Label htmlFor="k-ttl">Expires</Label>
              <Select value={ttl} onValueChange={setTtl}>
                <SelectTrigger id="k-ttl" className="w-36">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {TTLS.map(([label], i) => (
                    <SelectItem key={label} value={String(i)}>
                      {label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <Button type="submit" disabled={!policy || !name.trim() || issue.isPending}>
              Issue key
            </Button>
          </form>
        )}
      </CardContent>
    </Card>
  );
}

function keyState(k: KeyInfo): { label: string; variant: "success" | "secondary" | "destructive" | "warning" } {
  if (k.disabled) return { label: "Disabled", variant: "secondary" };
  if (k.expires_at && Date.parse(k.expires_at) <= Date.now()) return { label: "Expired", variant: "destructive" };
  if (k.previous_expires_at && Date.parse(k.previous_expires_at) > Date.now()) return { label: "Old key in grace", variant: "warning" };
  return { label: "Active", variant: "success" };
}

const WINDOWS = [
  ["requests_per_minute", "req/min"],
  ["requests_per_day", "req/day"],
  ["tokens_per_minute", "tok/min"],
  ["tokens_per_day", "tok/day"],
] as const;

/** Consumption against each limit of the key's policy. */
function LimitUsage({ k }: { k: KeyInfo }) {
  const limited = WINDOWS.filter(([w]) => k.limits?.[w] != null);
  if (!limited.length) return <span className="text-muted-foreground">none</span>;
  return (
    <div className="flex flex-col font-mono">
      {limited.map(([w, label]) => {
        const used = k.limit_usage?.[w] ?? 0;
        const limit = k.limits![w]!;
        return (
          <span key={w} className={used >= limit ? "text-destructive" : undefined}>
            {compact(used)}/{compact(limit)} {label}
          </span>
        );
      })}
      {k.limits?.per === "policy" && <span className="text-muted-foreground font-sans">shared by the policy</span>}
    </div>
  );
}

function KeyRow({ k, requests }: { k: KeyInfo; requests: number }) {
  const path = `/admin/keys/${k.policy}/${k.name}`;
  const inv = { invalidate: [["keys"]] };
  const toggle = useAction(() => post(`${path}/${k.disabled ? "enable" : "disable"}`), {
    ...inv,
    success: k.disabled ? "Key enabled" : "Key disabled",
  });
  const revoke = useAction(() => del(path), { ...inv, success: `Revoked ${k.client_id}` });
  const state = keyState(k);
  const grace = k.previous_expires_at && Date.parse(k.previous_expires_at) > Date.now();

  return (
    <TableRow>
      <TableCell>
        <Mono className="font-medium whitespace-nowrap">{k.client_id}</Mono>
        <div className="text-muted-foreground text-xs">
          {grace ? `Rotation grace ends ${relativeIso(k.previous_expires_at)}` : k.description}
        </div>
      </TableCell>
      <TableCell>
        <Badge variant={state.variant}>{state.label}</Badge>
      </TableCell>
      <TableCell className="font-mono text-xs whitespace-nowrap">{k.vault_path}</TableCell>
      <TableCell className="whitespace-nowrap">{relativeIso(k.rotated_at)}</TableCell>
      <TableCell className="whitespace-nowrap">{k.expires_at ? relativeIso(k.expires_at) : "never"}</TableCell>
      <TableCell className="text-right font-mono">{num(requests)}</TableCell>
      <TableCell className="text-xs whitespace-nowrap">
        <LimitUsage k={k} />
      </TableCell>
      <TableCell>
        <div className="flex justify-end gap-2">
          <RotateKey k={k} />
          <Button size="sm" variant="outline" disabled={toggle.isPending} onClick={() => toggle.mutate(undefined)}>
            {k.disabled ? "Enable" : "Disable"}
          </Button>
          <Confirm
            title={`Revoke ${k.client_id}?`}
            description="The key stops working at once and every stored version of it is destroyed. This can't be undone; issue a new key instead."
            action="Revoke"
            onConfirm={() => revoke.mutate(undefined)}
          >
            <Button size="sm" variant="outline" className="text-destructive" disabled={revoke.isPending}>
              Revoke
            </Button>
          </Confirm>
        </div>
      </TableCell>
    </TableRow>
  );
}

function RotateKey({ k }: { k: KeyInfo }) {
  const [open, setOpen] = useState(false);
  const [grace, setGrace] = useState("2");
  const rotate = useAction(() => post(`/admin/keys/${k.policy}/${k.name}/rotate`, { grace_secs: GRACES[Number(grace)][1] }), {
    invalidate: [["keys"]],
    success: `Rotated ${k.client_id}`,
    onSuccess: () => setOpen(false),
  });
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm" variant="outline">
          Rotate…
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Rotate {k.client_id}</DialogTitle>
          <DialogDescription>
            A new key is written to <Mono>{k.vault_path}</Mono>. During the grace period the old key keeps working, so the client can pick up the new one.
          </DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-2">
          <Label htmlFor={`grace-${k.client_id}`}>Grace period</Label>
          <Select value={grace} onValueChange={setGrace}>
            <SelectTrigger id={`grace-${k.client_id}`}>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {GRACES.map(([label], i) => (
                <SelectItem key={label} value={String(i)}>
                  {label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => setOpen(false)}>
            Cancel
          </Button>
          <Button disabled={rotate.isPending} onClick={() => rotate.mutate(undefined)}>
            Rotate
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
