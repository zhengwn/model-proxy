import { describe, it, expect } from "vitest";
import fc from "fast-check";
import {
  validateMaxBodyBytes,
  validateRetentionDays,
  MAX_BODY_BYTES_MIN,
  MAX_BODY_BYTES_MAX,
  RETENTION_DAYS_MIN,
  RETENTION_DAYS_MAX,
} from "../loggingValidation";

// Feature: request-logging, Property 12: Numeric config validation accepts correct range

describe("Property 12: Numeric config validation accepts correct range", () => {
  it("max_body_bytes validator accepts if and only if value is in [1024, 65536]", () => {
    fc.assert(
      fc.property(
        fc.integer({ min: -1000, max: 100000 }),
        (value) => {
          const result = validateMaxBodyBytes(value);
          const expected = value >= MAX_BODY_BYTES_MIN && value <= MAX_BODY_BYTES_MAX;
          expect(result).toBe(expected);
        }
      ),
      { numRuns: 200 }
    );
  });

  it("retention_days validator accepts if and only if value is in [1, 90]", () => {
    fc.assert(
      fc.property(
        fc.integer({ min: -1000, max: 100000 }),
        (value) => {
          const result = validateRetentionDays(value);
          const expected = value >= RETENTION_DAYS_MIN && value <= RETENTION_DAYS_MAX;
          expect(result).toBe(expected);
        }
      ),
      { numRuns: 200 }
    );
  });

  // **Validates: Requirements 6.4, 6.5**
});
