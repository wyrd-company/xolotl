import { useQuery } from "@tanstack/react-query";

import { QueryError } from "@/components/layout";
import { Button } from "@/components/ui/button";
import { get, type CatalogPricing } from "@/lib/api";
import { dateTime, relative } from "@/lib/format";

function cacheAge(seconds: number): string {
  const [unit, size] = seconds >= 86400 ? ["day", 86400] : seconds >= 3600 ? ["hour", 3600] : seconds >= 60 ? ["minute", 60] : ["second", 1];
  return new Intl.NumberFormat(undefined, { style: "unit", unit, unitDisplay: "long" }).format(Math.floor(seconds / size));
}

export function PricingFeedback({ refreshing }: { refreshing: boolean }) {
  const pricing = useQuery({
    queryKey: ["catalog-pricing"],
    queryFn: () => get<CatalogPricing>("/admin/catalog/pricing"),
    refetchInterval: 30_000,
  });
  const result = pricing.data;
  return (
    <section aria-label="Pricing" className="min-w-0">
      <h2 className="mb-2 font-medium">Pricing</h2>
      <QueryError error={pricing.error} />
      {pricing.error && <Button variant="outline" size="sm" onClick={() => void pricing.refetch()}>Retry pricing status</Button>}
      {result ? (
        <div role="status" className="flex flex-col gap-1 break-words">
          <p className={refreshing ? undefined : result.error ? "text-warning" : undefined}>
            {refreshing ? "Refreshing prices…" : !result.enabled ? "Registry refresh is disabled" : result.error ? "Latest pricing refresh failed" : result.attempted_at != null ? "Pricing refresh succeeded" : result.fetched_at != null ? "Prices loaded from cache" : "Prices have not been fetched"}
          </p>
          {result.attempted_at != null && <p className="text-muted-foreground">Latest attempt: <time dateTime={new Date(result.attempted_at * 1000).toISOString()}>{dateTime(result.attempted_at)} ({relative(result.attempted_at)})</time></p>}
          {!refreshing && result.error && <p>{result.error}</p>}
          {result.fetched_at != null ? (
            <p className="text-muted-foreground">Last good registry fetch: <time dateTime={new Date(result.fetched_at * 1000).toISOString()}>{dateTime(result.fetched_at)} ({relative(result.fetched_at)})</time>.{result.error || refreshing ? " Cached prices retained." : ""}</p>
          ) : <p className="text-muted-foreground">No cached registry prices are available. Prices are unknown unless set in configuration.</p>}
          {result.age_secs != null && <p className="text-muted-foreground">Cached registry prices are {cacheAge(result.age_secs)} old.</p>}
          {!refreshing && result.error && <p>Retry Refresh models.</p>}
        </div>
      ) : pricing.isPending ? <p role="status">Loading pricing status…</p> : null}
    </section>
  );
}
