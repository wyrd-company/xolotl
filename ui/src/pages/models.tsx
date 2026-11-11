// ---
// relationships:
//   implements: ../../../docs/user/web-ui.md
// ---
import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import { CatalogRefresh } from "@/components/catalog-refresh";

import { Endpoints, EndpointsLegend } from "@/components/endpoints";
import { Empty, PageHeader, QueryError } from "@/components/layout";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { get, type CatalogRoute } from "@/lib/api";
import { compact, relative } from "@/lib/format";

export function ModelsPage() {
  const routes = useQuery({ queryKey: ["catalog-routes"], queryFn: () => get<CatalogRoute[]>("/admin/catalog/routes") });

  return (
    <>
      <PageHeader
        title="Models"
        description={
          <>
            The models every account serves. Add providers and their keys on <Link to="/accounts">Accounts</Link>.
          </>
        }
      />
      <QueryError error={routes.error} />
      <Catalog routes={routes.data ?? []} />
    </>
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
      {shown.length === 0 && <Empty>{routes.length ? "No model matches." : "The catalog is empty. Add accounts, then refresh."}</Empty>}
    </Card>
  );
}

function price(rate: number | null | undefined) { return rate == null ? "Unknown" : `$${(rate * 1e6).toFixed(2)}`; }
