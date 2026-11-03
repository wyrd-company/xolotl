// ---
// relationships:
//   implements: ../../../docs/user/web-ui.md
// ---
import { useQuery } from "@tanstack/react-query";

import { Empty, QueryError } from "@/components/layout";
import { Badge } from "@/components/ui/badge";
import { Card } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { get, type SessionRow, type ToolUsageRow } from "@/lib/api";
import { compact, num, relative } from "@/lib/format";

/** An empty client is the admin token's filter: it has no sessions or tool counts, so it matches nothing. */
export const clientQuery = (client: string | undefined) => (client === undefined ? "" : `?client=${encodeURIComponent(client)}`);
const percent = (ratio: number | null | undefined) => (ratio == null ? "unknown" : `${Math.round(ratio * 100)}%`);

/** Sessions by most recent use: how full each one is and where it is locked. */
export function SessionsCard({ client }: { client?: string }) {
  const sessions = useQuery({ queryKey: ["sessions", client], queryFn: () => get<SessionRow[]>(`/admin/sessions${clientQuery(client)}`) });
  const rows = sessions.data ?? [];
  return (
    <section className="flex flex-col gap-2" aria-labelledby="sessions-heading">
      <h2 id="sessions-heading" className="text-lg font-semibold">
        Sessions
      </h2>
      <p className="text-muted-foreground text-sm">The 100 most recently used. A locked session stays on its account and model so that its prompt cache is kept.</p>
      <QueryError error={sessions.error} />
      <Card className="py-0">
        <Table className="min-w-[960px]">
          <TableHeader>
            <TableRow>
              <TableHead>Client</TableHead>
              <TableHead>Session</TableHead>
              <TableHead>Model</TableHead>
              <TableHead className="text-right">Context</TableHead>
              <TableHead className="text-right">Peak</TableHead>
              <TableHead className="text-right">Turns</TableHead>
              <TableHead className="text-right">Cache hits</TableHead>
              <TableHead>Lock</TableHead>
              <TableHead className="text-right">Last used</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody className="font-mono text-xs">
            {rows.map((s) => (
              <TableRow key={`${s.client_id}/${s.session_key}`}>
                <TableCell>{s.client_id}</TableCell>
                <TableCell title={`${s.session_key} (${s.source})`}>
                  {s.session_key.slice(0, 8)} <span className="text-muted-foreground font-sans">{s.source}</span>
                </TableCell>
                <TableCell>{s.provider ? `${s.provider}/${s.model}` : "-"}</TableCell>
                <TableCell className="text-right">
                  {compact(s.current_tokens)} / {s.context_window == null ? "?" : compact(s.context_window)} ({percent(s.utilization)})
                </TableCell>
                <TableCell className="text-right">
                  {compact(s.peak_tokens)} ({percent(s.peak_utilization)})
                </TableCell>
                <TableCell className="text-right">{num(s.turns)}</TableCell>
                <TableCell className="text-right">{percent(s.cache_hit_ratio)}</TableCell>
                <TableCell>
                  <Lock session={s} />
                </TableCell>
                <TableCell className="text-right font-sans">{relative(s.last_seen)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {sessions.isSuccess && rows.length === 0 && <Empty>No sessions yet.</Empty>}
      </Card>
    </section>
  );
}

function Lock({ session: s }: { session: SessionRow }) {
  return (
    <div className="flex flex-wrap items-center gap-1">
      {s.lock_provider ? (
        <Badge variant="secondary" title={s.lock_name ?? undefined}>
          {s.lock_account ?? `${s.lock_provider}/${s.lock_model}`}
        </Badge>
      ) : (
        <span className="text-muted-foreground font-sans">none</span>
      )}
      {s.lock_failures > 0 && <Badge variant="outline">{num(s.lock_failures)} failed</Badge>}
      {s.lock_breaks > 0 && (
        <Badge variant="outline" title={s.last_break_at == null ? undefined : relative(s.last_break_at)}>
          broken {num(s.lock_breaks)}× ({s.last_break_reason})
        </Badge>
      )}
    </div>
  );
}

/** The tools each client key called, for pruning its policy by hand. */
export function ToolsCard({ client }: { client?: string }) {
  const tools = useQuery({ queryKey: ["tools", client], queryFn: () => get<ToolUsageRow[]>(`/admin/tools${clientQuery(client)}`) });
  const rows = tools.data ?? [];
  return (
    <section className="flex flex-col gap-2" aria-labelledby="tools-heading">
      <h2 id="tools-heading" className="text-lg font-semibold">
        Tools called
      </h2>
      <p className="text-muted-foreground text-sm">Per client key, over the retained log. A tool that isn't listed wasn't called.</p>
      <QueryError error={tools.error} />
      <Card className="py-0">
        <Table className="min-w-[560px]">
          <TableHeader>
            <TableRow>
              <TableHead>Client</TableHead>
              <TableHead>Tool</TableHead>
              <TableHead>Through</TableHead>
              <TableHead className="text-right">Calls</TableHead>
              <TableHead className="text-right">Last used</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody className="font-mono text-xs">
            {rows.map((t) => (
              <TableRow key={`${t.client_id}/${t.source}/${t.tool}`}>
                <TableCell>{t.client_id}</TableCell>
                <TableCell>{t.tool}</TableCell>
                <TableCell className="font-sans">{t.source === "mcp" ? "/mcp" : "LLM response"}</TableCell>
                <TableCell className="text-right">{num(t.calls)}</TableCell>
                <TableCell className="text-right font-sans">{relative(t.last_used)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {tools.isSuccess && rows.length === 0 && <Empty>No tool calls yet.</Empty>}
      </Card>
    </section>
  );
}
