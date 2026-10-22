// ---
// relationships:
//   implements: ../../../docs/web-ui.md
// ---
import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { CatalogRefresh } from "@/components/catalog-refresh";

import { Endpoints, EndpointsLegend } from "@/components/endpoints";
import { Empty, Mono, PageHeader, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { get, post, type CatalogRoute, type GatewayProvider } from "@/lib/api";
import { compact, relative } from "@/lib/format";
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

  return (
    <>
      <PageHeader
        title="Providers & models"
        description="Providers come from the config file; their keys live in OpenBao."
      />
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
      <CardHeader className="has-data-[slot=card-action]:grid-cols-1 sm:has-data-[slot=card-action]:grid-cols-[1fr_auto]">
        <CardTitle>Model catalog</CardTitle>
        <CardDescription>
          {routes.length} models. Which of them a key sees, and under what name, is up to its policy.
        </CardDescription>
        <CardAction className="col-start-1 row-span-1 row-start-3 w-full justify-self-start sm:col-start-2 sm:row-span-2 sm:row-start-1 sm:w-auto sm:justify-self-end">
          <Label htmlFor="catalog-filter" className="sr-only">
            Filter models
          </Label>
          <Input id="catalog-filter" type="search" placeholder="Filter, e.g. haiku" className="w-full sm:w-56" value={filter} onChange={(e) => setFilter(e.target.value)} />
        </CardAction>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <CatalogRefresh />
        <EndpointsLegend />
      </CardContent>
      <Table className="min-w-[1100px]">
        <TableHeader>
          <TableRow>
            <TableHead>Model</TableHead>
            <TableHead>Provider</TableHead>
            <TableHead>Endpoints</TableHead>
            <TableHead>Context / max output</TableHead>
            <TableHead>API-equivalent / Mtok</TableHead>
            <TableHead>Metadata source / age</TableHead>
            <TableHead>Also served by</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {shown.map((r) => (
            <TableRow key={`${r.provider}/${r.model}`}>
              <TableCell className="font-mono whitespace-nowrap text-xs">{r.model}</TableCell>
              <TableCell className="font-mono text-xs">{r.provider}</TableCell>
              <TableCell>
                <Endpoints native={r.endpoints} translated={r.translated_endpoints} />
              </TableCell>
              <TableCell className="font-mono text-xs">
                {r.metadata?.max_input_tokens == null ? "Unknown" : compact(r.metadata.max_input_tokens)} / {r.metadata?.max_output_tokens == null ? "Unknown" : compact(r.metadata.max_output_tokens)}
              </TableCell>
              <TableCell className="font-mono text-xs">
                {price(r.metadata?.input_cost_per_token)} / {price(r.metadata?.output_cost_per_token)}
                <div className="text-muted-foreground">Cache read {price(r.metadata?.cache_read_input_token_cost)} / write {price(r.metadata?.cache_creation_input_token_cost)}</div>
              </TableCell>
              <TableCell className="max-w-72 break-all text-xs">
                {r.metadata?.source ?? "Unknown"}
                <div className="text-muted-foreground">{r.metadata?.fetched_at ? relative(r.metadata.fetched_at) : r.metadata?.source ? "Config override" : "No data"}</div>
                {r.metadata?.error && <div className="text-warning break-normal">Refresh failed; cached data retained</div>}
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

function price(rate: number | null | undefined) { return rate == null ? "Unknown" : `$${(rate * 1e6).toFixed(2)}`; }
