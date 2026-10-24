// ---
// relationships:
//   references: ../../../docs/user/accounts.md
// ---
import type { UsageWindow } from "@/lib/api";

/** Also filter snapshots written before the server stopped recording this internal bucket. */
export function publicUsageWindows(windows: UsageWindow[] | undefined): UsageWindow[] {
  return (windows ?? []).filter((w) => w.name !== "iguana_necktie");
}
