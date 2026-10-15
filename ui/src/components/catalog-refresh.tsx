// ---
// relationships:
//   references: docs/web-ui.md
// ---
import { useEffect } from "react";
import {
  useIsMutating,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";

import { QueryError } from "@/components/layout";
import { Button } from "@/components/ui/button";
import { get, post, type CatalogDiscovery } from "@/lib/api";
import { relative } from "@/lib/format";

/** Reuse beside model lists and counts; scope is provider-level, including account views. */
export function CatalogRefresh({ providers }: { providers?: string[] }) {
  const qc = useQueryClient();
  const status = useQuery({
    queryKey: ["catalog-status"],
    queryFn: () => get<CatalogDiscovery>("/admin/catalog/status"),
    refetchInterval: 30_000,
  });
  const checkedAt = status.dataUpdatedAt;
  useEffect(() => {
    if (!checkedAt) return;
    // Background discovery (including account changes) also updates existing lists.
    for (const key of ["catalog-routes", "policy-view", "status"])
      void qc.invalidateQueries({ queryKey: [key] });
  }, [qc, checkedAt]);
  const refreshing = useIsMutating({ mutationKey: ["catalog-refresh"] }) > 0;
  const refresh = useMutation({
    mutationKey: ["catalog-refresh"],
    mutationFn: () =>
      post(
        `/admin/catalog/refresh${providers ? `?providers=${encodeURIComponent(providers.join(","))}` : ""}`,
      ),
    onSuccess: async () => {
      await Promise.all(
        ["catalog-status", "catalog-routes", "status", "policy-view"].map(
          (key) => qc.invalidateQueries({ queryKey: [key] }),
        ),
      );
    },
  });
  const rows = Object.entries(status.data ?? {}).filter(
    ([provider]) => !providers || providers.includes(provider),
  );
  // A mixed list is as fresh as its oldest provider; never-refreshed providers stay visible.
  const refreshed =
    rows.length && rows.every(([, result]) => result.refreshed_at != null)
      ? Math.min(...rows.map(([, result]) => result.refreshed_at!))
      : null;

  return (
    <div className="flex flex-col gap-2 text-sm">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-muted-foreground">
          Catalog refreshed{" "}
          {status.isPending
            ? "loading…"
            : refreshed == null
              ? "not yet"
              : relative(refreshed)}
        </span>
        <span aria-hidden="true">·</span>
        <Button
          type="button"
          size="sm"
          variant="outline"
          disabled={refreshing || providers?.length === 0}
          onClick={() => refresh.mutate()}
        >
          <RefreshCw className={refreshing ? "animate-spin" : undefined} />
          {refreshing ? "Refreshing models…" : "Refresh models"}
        </Button>
      </div>
      <QueryError error={status.error ?? refresh.error} />
      <ul
        className="text-muted-foreground flex flex-col gap-1"
        aria-live="polite"
      >
        {rows.map(([provider, result]) => (
          <li key={provider}>
            <span className="font-mono">{provider}</span>: {result.models}{" "}
            models
            {result.error
              ? ` · Discovery failed: ${result.error}${result.models ? " (cached or static models shown)" : ""}`
              : result.refreshed_at == null
                ? " · Not discovered yet"
                : result.models === 0
                  ? " · Provider returned no models"
                  : ""}
            {result.refreshed_at != null && (
              <span> · refreshed {relative(result.refreshed_at)}</span>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}
