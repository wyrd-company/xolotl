/** CLI-style look-back durations. Bare numbers are seconds; UI windows must be positive. */
export function durationSeconds(value: string): number | null {
  const match = /^(\d+)([smhdw]?)$/.exec(value.trim());
  if (!match) return null;
  const multiplier: Record<string, number> = { "": 1, s: 1, m: 60, h: 3600, d: 86400, w: 604800 };
  const seconds = Number(match[1]) * multiplier[match[2]];
  return Number.isSafeInteger(seconds) && seconds > 0 ? seconds : null;
}

export function formatDuration(seconds: number): string {
  for (const [suffix, size] of [["w", 604800], ["d", 86400], ["h", 3600], ["m", 60]] as const) {
    if (seconds % size === 0) return `${seconds / size}${suffix}`;
  }
  return `${seconds}s`;
}

export const DURATION_ERROR = "Enter a positive whole-number duration in seconds or with s, m, h, d or w, such as 36h.";
