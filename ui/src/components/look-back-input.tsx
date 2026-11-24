import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

export function LookBackInput({ value, onChange, error, id = "look-back" }: {
  value: string;
  onChange: (value: string) => void;
  error?: string;
  id?: string;
}) {
  return (
    <div className="flex flex-col gap-2">
      <Label htmlFor={id}>Look back</Label>
      <Input id={id} value={value} onChange={(event) => onChange(event.target.value)}
        aria-invalid={!!error} aria-describedby={`${id}-hint${error ? ` ${id}-error` : ""}`} placeholder="36h" className="w-44" />
      <p id={`${id}-hint`} className="text-muted-foreground text-sm">Seconds, or s, m, h, d, w.</p>
      {error && <p id={`${id}-error`} role="alert" className="text-destructive text-sm">{error}</p>}
    </div>
  );
}
