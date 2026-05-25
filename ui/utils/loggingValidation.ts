/**
 * Validation functions for logging configuration fields.
 * Exported for use in property-based tests (task 9.2).
 */

export const MAX_BODY_BYTES_MIN = 1024;
export const MAX_BODY_BYTES_MAX = 65536;
export const RETENTION_DAYS_MIN = 1;
export const RETENTION_DAYS_MAX = 90;

/**
 * Validates that max_body_bytes is within the allowed range [1024, 65536].
 * Returns true if the value is valid.
 */
export function validateMaxBodyBytes(value: number): boolean {
  return (
    Number.isInteger(value) &&
    value >= MAX_BODY_BYTES_MIN &&
    value <= MAX_BODY_BYTES_MAX
  );
}

/**
 * Validates that retention_days is within the allowed range [1, 90].
 * Returns true if the value is valid.
 */
export function validateRetentionDays(value: number): boolean {
  return (
    Number.isInteger(value) &&
    value >= RETENTION_DAYS_MIN &&
    value <= RETENTION_DAYS_MAX
  );
}
