const units: [Intl.RelativeTimeFormatUnit, number][] = [
  ["day", 86_400],
  ["hour", 3_600],
  ["minute", 60],
];

const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

/** "3 min ago", "in 2 hours" for a unix timestamp in seconds. */
export function relative(ts: number | null | undefined): string {
  if (ts == null) return "-";
  const diff = ts - Date.now() / 1000;
  const abs = Math.abs(diff);
  if (abs < 45) return diff < 0 ? "just now" : "in a moment";
  for (const [unit, secs] of units) {
    if (abs >= secs || unit === "minute") return rtf.format(Math.round(diff / secs), unit);
  }
  return "-";
}

export function relativeIso(iso: string | null | undefined): string {
  return iso ? relative(Date.parse(iso) / 1000) : "-";
}

export function clock(ts: number): string {
  return new Date(ts * 1000).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}

export function dateTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

const nf = new Intl.NumberFormat();
export const num = (n: number) => nf.format(n);

const compactNf = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });
/** 1234 -> "1.2K". */
export const compact = (n: number) => compactNf.format(n);

/** USD per million tokens. Keep small nonzero rates visible instead of rounding to zero. */
export function price(rate: number | null | undefined, minimumFractionDigits = 4): string {
  if (rate == null) return "Unknown";
  const perMillion = rate * 1e6;
  if (perMillion > 0 && perMillion < 1e-100) return `$${perMillion.toPrecision(12)}`;
  return `$${new Intl.NumberFormat(undefined, {
    minimumFractionDigits,
    maximumFractionDigits: Math.min(100, Math.max(4, 11 - Math.floor(Math.log10(perMillion || 1)))),
  }).format(perMillion)}`;
}

/** Endpoint paths shortened for tables: /v1/chat/completions -> chat. */
export function endpointName(path: string): string {
  return path.replace(/^\/v1\//, "").replace("chat/completions", "chat");
}
