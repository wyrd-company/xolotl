/** Validate without converting through JavaScript Number. */
export function priorityError(input: string): string | null {
  const value = input.trim();
  if (/^-?\d+$/.test(value)) {
    const number = BigInt(value);
    if (number >= -9223372036854775808n && number <= 9223372036854775807n) return null;
  }
  return "Enter a whole number from −9223372036854775808 to 9223372036854775807.";
}

/** Call only after validation. Canonical decimal form keeps equality exact. */
export function normalizePriority(input: string): string {
  return BigInt(input.trim()).toString();
}
