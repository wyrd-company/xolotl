import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";

import { Endpoints, EndpointsLegend } from "@/components/endpoints";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { get, post, type CatalogRoute, type GatewayProvider, type Status } from "@/lib/api";
import { relative } from "@/lib/format";
import { useAction } from "@/lib/hooks";
import { KeyState } from "@/pages/overview";

const KINDS: Record<GatewayProvider["kind"], string> = {
  vercel: "Vercel AI Gateway",
  mistral: "Mistral",
  "openai-compatible": "OpenAI-compatible",
};

export function ProvidersPage() {
  const providers = useQuery({ queryKey: ["providers"], queryFn: () => get<GatewayProvider[]>("/admin/providers") });
  const routes = useQuery({ queryKey: ["catalog-routes"], queryFn: () => get<CatalogRoute[]>("/admin/catalog/routes") });
  const status = useQuery({ queryKey: ["status"], queryFn: () => get<Status>("/admin/status") });
  const refresh = useAction(() => post("/admin/catalog/refresh"), {
    invalidate: [["catalog-routes"], ["status"], ["policy-view"]],
    success: "Catalog refreshed",
  });
  const ago = status.data?.catalog_refreshed_secs_ago;

  return (
    <>
      <PageHeader
        title="Providers & models"
        description={
          <>
            Providers come from the config file; their keys live in OpenBao. Catalog refreshed{" "}
            {ago == null ? "not yet" : relative(Date.now() / 1000 - ago)}.
          </>
        }
      >
        <Button variant="outline" disabled={refresh.isPending} onClick={() => refresh.mutate(undefined)}>
          <RefreshCw className={refresh.isPending ? "animate-spin" : undefined} />
          Refresh catalog
        </Button>
      </PageHeader>
      <QueryError error={providers.error ?? routes.error} />

      <section aria-label="Gateway providers" className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        {(providers.data ?? []).map((p) => (
          <ProviderCard key={p.name} p={p} />
        ))}
        {providers.data?.length === 0 && (
          <p className="text-muted-foreground text-sm">No gateway providers are configured.</p>
        )}
      </section>

      <Catalog routes={routes.data ?? []} />
    </>
  );
}

function ProviderCard({ p }: { p: GatewayProvider }) {
  const [key, setKey] = useState("");
  const save = useAction(() => post(`/admin/providers/${p.name}/key`, { api_key: key.trim() }), {
    invalidate: [["providers"]],
    success: `Saved the ${p.name} key`,
    onSuccess: () => setKey(""),
  });
  const id = `key-${p.name}`;
  return (
    <Card className="gap-4">
      <CardHeader>
        <CardTitle className="font-mono">{p.name}</CardTitle>
        <CardDescription>{KINDS[p.kind]}</CardDescription>
        {!p.enabled && (
          <CardAction>
            <Badge variant="secondary">disabled</Badge>
          </CardAction>
        )}
      </CardHeader>
      <CardContent className="flex flex-col gap-3 text-sm">
        <div>
          Key: <KeyState provider={p} />
          {!p.no_auth && (
            <>
              {" "}
              at <Mono>{p.secret}</Mono>
            </>
          )}
        </div>
        {p.base_url && <Mono className="text-muted-foreground text-xs break-all">{p.base_url}</Mono>}
        {!p.no_auth && p.enabled && (
          <form
            className="flex flex-col gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              save.mutate(undefined);
            }}
          >
            <Label htmlFor={id}>{p.has_key ? "Replace API key" : "Set API key"}</Label>
            <div className="flex gap-2">
              <Input id={id} type="password" autoComplete="off" className="font-mono" value={key} onChange={(e) => setKey(e.target.value)} />
              <Button type="submit" variant="outline" disabled={!key.trim() || save.isPending}>
                Save
              </Button>
            </div>
          </form>
        )}
      </CardContent>
    </Card>
  );
}

function Catalog({ routes }: { routes: CatalogRoute[] }) {
  const [filter, setFilter] = useState("");
  const servedBy = useMemo(() => {
    const m = new Map<string, string[]>();
    for (const r of routes) m.set(r.model, [...(m.get(r.model) ?? []), r.provider]);
    return m;
  }, [routes]);
  const f = filter.trim().toLowerCase();
  const shown = f ? routes.filter((r) => `${r.provider}/${r.model}`.toLowerCase().includes(f)) : routes;

  return (
    <Card className="gap-4 pb-0">
      <CardHeader>
        <CardTitle>Model catalog</CardTitle>
        <CardDescription>
          {routes.length} models. Which of them a key sees, and under what name, is up to its policy.
        </CardDescription>
        <CardAction>
          <Label htmlFor="catalog-filter" className="sr-only">
            Filter models
          </Label>
          <Input id="catalog-filter" type="search" placeholder="Filter, e.g. haiku" className="w-56" value={filter} onChange={(e) => setFilter(e.target.value)} />
        </CardAction>
      </CardHeader>
      <CardContent>
        <EndpointsLegend />
      </CardContent>
      <Table className="min-w-[760px]">
        <TableHeader>
          <TableRow>
            <TableHead>Model</TableHead>
            <TableHead>Provider</TableHead>
            <TableHead>Endpoints</TableHead>
            <TableHead>Also served by</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {shown.map((r) => (
            <TableRow key={`${r.provider}/${r.model}`}>
              <TableCell className="font-mono text-xs">{r.model}</TableCell>
              <TableCell className="font-mono text-xs">{r.provider}</TableCell>
              <TableCell>
                <Endpoints native={r.endpoints} translated={r.translated_endpoints} />
              </TableCell>
              <TableCell className="font-mono text-xs">
                {(servedBy.get(r.model) ?? []).filter((p) => p !== r.provider).join(", ") || "-"}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      {shown.length === 0 && <Empty>{routes.length ? "No model matches." : "The catalog is empty. Add accounts or provider keys, then refresh."}</Empty>}
    </Card>
  );
}
