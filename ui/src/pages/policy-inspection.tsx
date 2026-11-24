import { Empty, Mono } from "@/components/layout";
import { Section, SectionContent } from "@/components/section";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import type { PolicyView } from "@/lib/api";

export function SavedPolicyWarnings({ warnings }: { warnings: string[] }) {
  if (!warnings.length) return null;
  return (
    <Alert variant="warning" role="status" aria-label="Saved policy warnings">
      <AlertTitle>Policy saved with warnings</AlertTitle>
      <AlertDescription><ul className="list-disc pl-5">{warnings.map((warning, i) => <li key={i}>{warning}</li>)}</ul></AlertDescription>
    </Alert>
  );
}

/** Server-resolved draft, including aliases and qualified names, just like policies view. */
export function PolicyInspection({ view, error, pending, retry }: { view?: PolicyView; error: Error | null; pending: boolean; retry: () => void }) {
  return (
    <Section id="policy-routes" title="Resolved routes" description="The model names keys see and their ordered targets. Endpoints are those of the first target. This preview includes unsaved changes.">
      <SectionContent className="flex flex-col gap-4" aria-busy={pending}>
        {pending && <p role="status" className="text-muted-foreground text-sm">Resolving policy…</p>}
        {error ? (
          <Alert variant="destructive" role="alert"><AlertTitle>Could not resolve policy</AlertTitle><AlertDescription><p>{error.message}</p><Button type="button" variant="outline" disabled={pending} onClick={retry}>Retry</Button></AlertDescription></Alert>
        ) : view && (
          <>
            {view.models.length ? (
              <Table aria-label="Resolved model routes">
                <TableHeader><TableRow><TableHead>Model name</TableHead><TableHead>Routes to (in order)</TableHead><TableHead>Native endpoints</TableHead><TableHead>Translated endpoints</TableHead></TableRow></TableHeader>
                <TableBody>{view.models.map((model) => (
                  <TableRow key={model.name}>
                    <TableCell><Mono className="break-all">{model.name}</Mono> {model.alias && <Badge variant="outline">alias</Badge>}</TableCell>
                    <TableCell><ol className="list-decimal pl-5">{model.targets.map((target, i) => <li key={i}><Mono className="break-all">{target}</Mono></li>)}</ol></TableCell>
                    <TableCell>{model.endpoints.length ? model.endpoints.join(", ") : "—"}</TableCell>
                    <TableCell>{model.translated_endpoints.length ? model.translated_endpoints.join(", ") : "—"}</TableCell>
                  </TableRow>
                ))}</TableBody>
              </Table>
            ) : <Empty>No resolved model routes. Choose providers and allow models to expose routes.</Empty>}
            <h3 className="text-sm font-semibold">Subscription accounts</h3>
            {view.accounts === null ? (
              <Alert variant="destructive" role="alert"><AlertTitle>Could not read the accounts</AlertTitle><AlertDescription><p>Account availability is unknown. The resolved routes can still be inspected.</p><Button type="button" variant="outline" disabled={pending} onClick={retry}>Retry accounts</Button></AlertDescription></Alert>
            ) : Object.keys(view.accounts).length ? (
              <dl className="grid gap-x-4 gap-y-2 text-sm sm:grid-cols-[auto_1fr]">{Object.entries(view.accounts).map(([provider, ids]) => (
                <div key={provider} className="contents"><dt className="font-mono">{provider}</dt><dd className="break-all">{ids.length ? ids.join(", ") : "No accounts"}</dd></div>
              ))}</dl>
            ) : <p className="text-muted-foreground text-sm">No subscription providers allowed.</p>}
          </>
        )}
      </SectionContent>
    </Section>
  );
}
