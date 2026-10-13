import { Badge } from "@/components/ui/badge";
import { endpointName } from "@/lib/format";

/** Native endpoints as solid badges, translated ones as outlined badges. */
export function Endpoints({ native, translated }: { native: string[]; translated: string[] }) {
  return (
    <div className="flex flex-wrap gap-1">
      {native.map((e) => (
        <Badge key={e} variant="secondary" className="font-mono" title={`${e}: native`}>
          {endpointName(e)}
        </Badge>
      ))}
      {translated.map((e) => (
        <Badge key={e} variant="outline" className="text-muted-foreground border-dashed font-mono" title={`${e}: translated`}>
          {endpointName(e)}
        </Badge>
      ))}
    </div>
  );
}

export function EndpointsLegend() {
  return (
    <p className="text-muted-foreground flex flex-wrap items-center gap-2 text-xs">
      <Badge variant="secondary" className="font-mono">
        native
      </Badge>
      the provider speaks the format
      <Badge variant="outline" className="text-muted-foreground border-dashed font-mono">
        translated
      </Badge>
      xolotl translates requests and responses. <span className="font-mono">count_tokens</span> and{" "}
      <span className="font-mono">responses/compact</span> are native only.
    </p>
  );
}
