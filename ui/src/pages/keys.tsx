import { useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { Confirm } from "@/components/confirm";
import { Empty, Mono, PageHeader } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { del, get, post, type KeyInfo, type Policy, type RevokeResult, type Status, type UsageRow } from "@/lib/api";
import { dateTime, num } from "@/lib/format";
import { useAction } from "@/lib/hooks";
import { keyDuration, revokeMessage } from "@/lib/key-lifecycle";

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

function QueryFailure({ title, error, retry, pending }: { title: string; error: Error | null; retry: () => void; pending: boolean }) {
  if (!error) return null;
  return (
    <div role="alert" className="bg-danger-muted text-destructive rounded-md px-4 py-3 text-sm">
      <p className="font-medium">{title}</p>
      <p>{error.message}</p>
      <Button variant="outline" size="sm" className="mt-2" disabled={pending} onClick={retry}>
        {pending ? "Retrying…" : "Retry"}
      </Button>
    </div>
  );
}

export function KeysPage() {
  const keys = useQuery({
    queryKey: ["keys"],
    queryFn: () => get<KeyInfo[]>("/admin/keys"),
  });
  const status = useQuery({
    queryKey: ["status"],
    queryFn: () => get<Status>("/admin/status"),
  });
  const usage = useQuery({
    queryKey: ["usage", "client", WEEK],
    queryFn: () => get<UsageRow[]>(`/admin/usage?group_by=client&since_secs=${WEEK}`),
  });
  const [issued, setIssued] = useState<KeyInfo | null>(null);
  const [revoked, setRevoked] = useState<RevokeResult | null>(null);
  const requests = new Map((usage.data ?? []).map((r) => [r.key, r.requests]));

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
            <CardTitle className="text-success text-base">
              {issued.status === "exists" ? "Exists" : "Issued"} {issued.client_id}
            </CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-2 text-sm">
            <p>Expires: {issued.expires_at ? timestamp(issued.expires_at) : "never"}. The key is stored in OpenBao.</p>
            {status.isSuccess && status.data ? (
              <>
                <div>The client reads it with:</div>
                <pre className="bg-card overflow-x-auto rounded-md border p-3 font-mono text-xs">
                  bao kv get -field=api_key {`${status.data.vault_mount}/${status.data.vault_prefix}/${issued.vault_path}`}
                </pre>
                <div>Grant it this Vault policy:</div>
                <pre className="bg-card overflow-x-auto rounded-md border p-3 font-mono text-xs">{`path "${status.data.vault_mount}/data/${status.data.vault_prefix}/${issued.vault_path}" { capabilities = ["read"] }`}</pre>
              </>
            ) : (
              <p>Vault retrieval details unavailable. Load deployment status to obtain the correct mount and prefix.</p>
            )}
            <Button variant="ghost" size="sm" className="self-start" onClick={() => setIssued(null)}>
              Dismiss
            </Button>
          </CardContent>
        </Card>
      )}
      <QueryFailure
        title="Could not load deployment status. Vault retrieval details are unavailable."
        error={status.error}
        pending={status.isFetching}
        retry={() => void status.refetch()}
      />
      <QueryFailure title="Could not load client keys" error={keys.error} pending={keys.isFetching} retry={() => void keys.refetch()} />
      <QueryFailure
        title="Could not load key usage for the last 7 days"
        error={usage.error}
        pending={usage.isFetching}
        retry={() => void usage.refetch()}
      />
      <Card className="py-0">
        <Table className="min-w-[920px]">
          <TableHeader>
            <TableRow>
              <TableHead>Client / description</TableHead>
              <TableHead>State / timestamps</TableHead>
              <TableHead>Vault path</TableHead>
              <TableHead>Current usage</TableHead>
              <TableHead className="text-right">Requests (7d)</TableHead>
              <TableHead>
                <span className="sr-only">Actions</span>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(keys.data ?? []).map((k) => (
              <KeyRow key={k.client_id} k={k} onRevoked={setRevoked} requests={usage.isSuccess ? (requests.get(k.client_id) ?? 0) : null} />
            ))}
          </TableBody>
        </Table>
        {keys.isPending && <Empty>Loading client keys…</Empty>}
        {keys.isSuccess && keys.data.length === 0 && <Empty>No client keys yet. Issue a key above.</Empty>}
      </Card>
      <RevokeById result={revoked} onRevoked={setRevoked} />
    </>
  );
}

function IssueKey({ onIssued }: { onIssued: (k: KeyInfo) => void }) {
  const policies = useQuery({
    queryKey: ["policies"],
    queryFn: () => get<Policy[]>("/admin/policies"),
  });
  const [policy, setPolicy] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [ttl, setTtl] = useState("0");
  const [custom, setCustom] = useState("");
  const [error, setError] = useState<string | null>(null);
  const issue = useAction(
    (ttl_secs: number | null) =>
      post<KeyInfo>("/admin/keys", {
        policy,
        name: name.trim(),
        description: description.trim(),
        ttl_secs,
      }),
    {
      invalidate: [["keys"]],
      success: (k) => (k.status === "exists" ? `${k.client_id} already has a live key. It was not changed.` : ""),
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
      <CardContent className="flex flex-col gap-3">
        <QueryFailure
          title="Could not load policies. Key issuance is unavailable."
          error={policies.error}
          pending={policies.isFetching}
          retry={() => void policies.refetch()}
        />
        {policies.isPending && <p className="text-muted-foreground text-sm">Loading policies…</p>}
        {noPolicies ? (
          <p className="text-muted-foreground text-sm">Create a policy first: every key belongs to one.</p>
        ) : (
          <form
            className="flex flex-wrap items-end gap-4"
            onSubmit={(e) => {
              e.preventDefault();
              try {
                const seconds = ttl === "custom" ? keyDuration(custom) : TTLS[Number(ttl)][1];
                setError(null);
                issue.mutate(seconds);
              } catch (e) {
                setError((e as Error).message);
              }
            }}
          >
            <div className="flex flex-col gap-2">
              <Label htmlFor="k-policy">Policy</Label>
              <Select value={policy} onValueChange={setPolicy} disabled={!policies.isSuccess}>
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
              <Label htmlFor="k-desc">Description (optional)</Label>
              <Input id="k-desc" value={description} onChange={(e) => setDescription(e.target.value)} />
            </div>
            <div className="flex flex-col gap-2">
              <Label htmlFor="k-ttl">Expires</Label>
              <Select
                value={ttl}
                onValueChange={(v) => {
                  setTtl(v);
                  setError(null);
                }}
              >
                <SelectTrigger id="k-ttl" className="w-36">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {TTLS.map(([label], i) => (
                    <SelectItem key={label} value={String(i)}>
                      {label}
                    </SelectItem>
                  ))}
                  <SelectItem value="custom">Custom</SelectItem>
                </SelectContent>
              </Select>
            </div>
            {ttl === "custom" && (
              <div className="flex flex-col gap-2">
                <Label htmlFor="k-custom-ttl">Custom expiry</Label>
                <Input
                  id="k-custom-ttl"
                  aria-invalid={!!error}
                  aria-describedby="k-ttl-help k-ttl-error"
                  className="w-48"
                  value={custom}
                  onChange={(e) => setCustom(e.target.value)}
                  placeholder="36h"
                />
              </div>
            )}
            <Button type="submit" disabled={!policies.isSuccess || !policy || !name.trim() || issue.isPending}>
              {issue.isPending ? "Issuing…" : "Issue key"}
            </Button>
            <p id="k-ttl-help" className="text-muted-foreground w-full text-sm">
              Default expiry: Never. Custom durations use s, m, h, d or w; without a unit, seconds.
            </p>
            {error && (
              <p id="k-ttl-error" role="alert" className="text-destructive w-full text-sm">
                {error}
              </p>
            )}
          </form>
        )}
      </CardContent>
    </Card>
  );
}

function timestamp(iso: string | null | undefined) {
  return iso ? dateTime(Date.parse(iso) / 1000) : "—";
}
function keyState(k: KeyInfo): {
  label: string;
  variant: "success" | "secondary" | "destructive" | "warning";
} {
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

/** Limited windows use the counted policy total; unbounded windows show this key's usage. */
function LimitUsage({ k }: { k: KeyInfo }) {
  return (
    <div className="flex flex-col font-mono">
      {WINDOWS.map(([w, label]) => {
        const limit = k.limits?.[w];
        const used = limit == null ? k.usage?.[w] : k.limit_usage?.[w];
        return (
          <span key={w} className={limit != null && used != null && used >= limit ? "text-destructive" : undefined}>
            {used == null ? "unavailable" : num(used)}
            {limit != null && `/${num(limit)}`} {label}
          </span>
        );
      })}
      {k.limits?.per === "policy" && <span className="text-muted-foreground font-sans">limited windows shared by the policy</span>}
    </div>
  );
}

function KeyRow({ k, requests, onRevoked }: { k: KeyInfo; requests: number | null; onRevoked: (r: RevokeResult) => void }) {
  const path = `/admin/keys/${k.policy}/${k.name}`;
  const toggle = useAction(() => post(`${path}/${k.disabled ? "enable" : "disable"}`), {
    invalidate: [["keys"]],
    success: k.disabled ? "Key enabled" : "Key disabled",
  });
  const revoke = useAction(() => del<RevokeResult>(path), {
    invalidate: [["keys"]],
    success: revokeMessage,
    onSuccess: onRevoked,
  });
  const state = keyState(k);
  const grace = k.previous_expires_at && Date.parse(k.previous_expires_at) > Date.now();
  return (
    <TableRow>
      <TableCell>
        <Mono className="font-medium whitespace-nowrap">{k.client_id}</Mono>
        <div className="text-muted-foreground text-sm">{k.description}</div>
      </TableCell>
      <TableCell className="min-w-56 max-w-64 text-sm">
        <Badge variant={state.variant}>{state.label}</Badge>
        {grace && <div>Grace ends {timestamp(k.previous_expires_at)}</div>}
        <div>Created {timestamp(k.created_at)}</div>
        <div>
          {k.rotated_at ? `Rotated ${timestamp(k.rotated_at)}` : `Rotation reference: created ${timestamp(k.created_at)} (never rotated)`}
        </div>
        <div>Expires: {k.expires_at ? timestamp(k.expires_at) : "never"}</div>
      </TableCell>
      <TableCell className="font-mono text-xs whitespace-nowrap">{k.vault_path}</TableCell>
      <TableCell className="text-sm whitespace-nowrap">
        <LimitUsage k={k} />
      </TableCell>
      <TableCell className="text-right font-mono">{requests == null ? "Unavailable" : num(requests)}</TableCell>
      <TableCell>
        <div className="flex justify-end gap-2">
          <RotateKey k={k} />
          <Button size="sm" variant="outline" disabled={toggle.isPending} onClick={() => toggle.mutate(undefined)}>
            {toggle.isPending ? "Saving…" : k.disabled ? "Enable" : "Disable"}
          </Button>
          <Confirm
            title={`Revoke ${k.client_id}?`}
            description="The key stops working at once and every stored version of it is destroyed. This can't be undone; issue a new key instead."
            action="Revoke"
            onConfirm={() => revoke.mutate(undefined)}
          >
            <Button size="sm" variant="outline" className="text-destructive" disabled={revoke.isPending}>
              {revoke.isPending ? "Revoking…" : "Revoke"}
            </Button>
          </Confirm>
        </div>
      </TableCell>
    </TableRow>
  );
}

function RotateKey({ k }: { k: KeyInfo }) {
  const [open, setOpen] = useState(false);
  const [grace, setGrace] = useState("0");
  const [custom, setCustom] = useState("0s");
  const [error, setError] = useState<string | null>(null);
  const rotate = useAction((grace_secs: number) => post(`/admin/keys/${k.policy}/${k.name}/rotate`, { grace_secs }), {
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
        <form
          className="flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            try {
              const seconds = grace === "custom" ? keyDuration(custom, true) : GRACES[Number(grace)][1];
              setError(null);
              rotate.mutate(seconds);
            } catch (e) {
              setError((e as Error).message);
            }
          }}
        >
          <DialogHeader>
            <DialogTitle>Rotate {k.client_id}</DialogTitle>
            <DialogDescription>
              A new key is written to <Mono>{k.vault_path}</Mono>. During the grace period the old key keeps working, so the client can pick
              up the new one.
            </DialogDescription>
          </DialogHeader>
          <div className="flex flex-col gap-2">
            <Label htmlFor={`grace-${k.client_id}`}>Grace period</Label>
            <Select
              value={grace}
              onValueChange={(v) => {
                setGrace(v);
                setError(null);
              }}
            >
              <SelectTrigger id={`grace-${k.client_id}`}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {GRACES.map(([label], i) => (
                  <SelectItem key={label} value={String(i)}>
                    {label}
                  </SelectItem>
                ))}
                <SelectItem value="custom">Custom</SelectItem>
              </SelectContent>
            </Select>
          </div>
          {grace === "custom" && (
            <div className="flex flex-col gap-2">
              <Label htmlFor={`custom-grace-${k.client_id}`}>Custom grace</Label>
              <Input
                id={`custom-grace-${k.client_id}`}
                value={custom}
                onChange={(e) => setCustom(e.target.value)}
                aria-invalid={!!error}
                aria-describedby="grace-help grace-error"
              />
            </div>
          )}
          <p id="grace-help" className="text-muted-foreground text-sm">
            Default: zero. Custom durations use s, m, h, d or w; without a unit, seconds. Zero stops the old key immediately.
          </p>
          {error && (
            <p id="grace-error" role="alert" className="text-destructive text-sm">
              {error}
            </p>
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={rotate.isPending}>
              {rotate.isPending ? "Rotating…" : "Rotate"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function RevokeById({ result, onRevoked }: { result: RevokeResult | null; onRevoked: (r: RevokeResult) => void }) {
  const [id, setId] = useState("");
  const trimmed = id.trim();
  const valid = /^[a-z0-9_-]{1,63}\/[a-z0-9_-]{1,63}$/.test(trimmed);
  const revoke = useAction(() => del<RevokeResult>(`/admin/keys/${trimmed}`), {
    invalidate: [["keys"]],
    success: revokeMessage,
    onSuccess: onRevoked,
  });
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Revoke by client ID</CardTitle>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <p className="text-muted-foreground text-sm">Revoke deletes the stored key. You can repeat a request even after its row is gone.</p>
        <div className="flex flex-wrap items-end gap-3">
          <div className="flex flex-col gap-2">
            <Label htmlFor="revoke-id">Client ID</Label>
            <Input
              id="revoke-id"
              value={id}
              placeholder="team/editor"
              onChange={(e) => setId(e.target.value)}
              aria-describedby="revoke-help"
            />
          </div>
          <Confirm
            title={`Revoke ${trimmed}?`}
            description="The key stops working at once and every stored version of it is destroyed. This can't be undone; issue a new key instead."
            action="Revoke"
            onConfirm={() => revoke.mutate(undefined)}
          >
            <Button variant="outline" className="text-destructive" disabled={!valid || revoke.isPending}>
              {revoke.isPending ? "Revoking…" : "Revoke…"}
            </Button>
          </Confirm>
        </div>
        <p id="revoke-help" className="text-muted-foreground text-sm">
          Use policy/name. Each part is 1–63 lowercase letters, digits, hyphens or underscores.
        </p>
        {result && <p role="status">{revokeMessage(result)}</p>}
      </CardContent>
    </Card>
  );
}
