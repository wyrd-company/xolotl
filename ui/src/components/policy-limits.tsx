import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import type { Consumption, Limits } from "@/lib/api";
import { num } from "@/lib/format";

const FIELDS = [
  ["requests_per_minute", "Requests per minute"],
  ["requests_per_day", "Requests per day"],
  ["tokens_per_minute", "Tokens per minute"],
  ["tokens_per_day", "Tokens per day"],
] as const;

/** Rate limits and token budgets of a policy. `usage` is the policy's current consumption (all keys together). */
export function PolicyLimits({ limits, onChange, usage }: { limits: Limits; onChange: (l: Limits) => void; usage?: Consumption }) {
  function setField(field: (typeof FIELDS)[number][0], raw: string) {
    const next = { ...limits };
    const n = Number(raw);
    if (raw.trim() === "" || !Number.isFinite(n)) delete next[field];
    else next[field] = Math.max(0, Math.floor(n));
    onChange(next);
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Rate limits</CardTitle>
        <CardDescription>
          Limits on the LLM endpoints over a sliding minute and day; empty is unlimited. Tokens are input plus output as the upstream reports them,
          counted when a response finishes. A request over a limit gets 429 with <code>retry-after</code>.
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        <div className="flex max-w-sm flex-col gap-2">
          <Label htmlFor="limits-per">Counted</Label>
          <Select value={limits.per ?? "key"} onValueChange={(v) => onChange({ ...limits, per: v === "policy" ? "policy" : undefined })}>
            <SelectTrigger id="limits-per">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="key">per key: each key has its own allowance</SelectItem>
              <SelectItem value="policy">per policy: all keys share one allowance</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
          {FIELDS.map(([field, label]) => (
            <div key={field} className="flex flex-col gap-2">
              <Label htmlFor={`limits-${field}`}>{label}</Label>
              <Input
                id={`limits-${field}`}
                type="number"
                min={1}
                step={1}
                inputMode="numeric"
                className="font-mono"
                placeholder="unlimited"
                value={limits[field] ?? ""}
                onChange={(e) => setField(field, e.target.value)}
              />
              {usage && <span className="text-muted-foreground text-xs">all keys now: {num(usage[field])}</span>}
            </div>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}
