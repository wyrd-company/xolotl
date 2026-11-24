import type { RevokeResult } from "./api";

/** Each vault-path segment follows the server's valid_name contract. */
export function validClientId(value: string): boolean {
  return /^[a-z0-9][a-z0-9_-]{0,62}\/[a-z0-9][a-z0-9_-]{0,62}$/.test(value);
}

/** Durations sent to the API must remain exact JSON integers. */
export function keyDuration(value: string, allowZero = false): number {
  const match = /^(\d+)([smhdw]?)$/.exec(value.trim());
  const units: Record<string, number> = {
    "": 1,
    s: 1,
    m: 60,
    h: 3600,
    d: 86400,
    w: 604800,
  };
  const seconds = match ? Number(match[1]) * units[match[2]] : NaN;
  if (!Number.isSafeInteger(seconds) || seconds < (allowZero ? 0 : 1)) {
    throw new Error(
      allowZero
        ? "Enter zero or a positive duration, for example 0s or 36h. Use s, m, h, d or w."
        : "Enter a positive duration, for example 36h. Use s, m, h, d or w, or choose Never.",
    );
  }
  return seconds;
}

export function revokeMessage(result: RevokeResult): string {
  switch (result.status) {
    case "revoked":
      return `Revoked ${result.revoked}.`;
    case "already_revoked":
      return `Already revoked ${result.revoked}.`;
    case "absent":
      return `No key found for ${result.revoked}.`;
  }
}
