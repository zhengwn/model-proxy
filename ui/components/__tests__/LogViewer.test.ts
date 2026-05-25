import { describe, it, expect } from "vitest";
import fc from "fast-check";
import type { LogEntry } from "../../types";
import { filterLogEntries, type StatusFilter } from "../../utils/logFilters";

// --- Generators ---

const logEntryArb: fc.Arbitrary<LogEntry> = fc.record({
  id: fc.string({ minLength: 1, maxLength: 20 }),
  timestamp: fc.string({ minLength: 1, maxLength: 30 }),
  method: fc.constantFrom("GET", "POST", "PUT", "DELETE", "PATCH"),
  path: fc.string({ minLength: 1, maxLength: 100 }),
  provider: fc.string({ minLength: 1, maxLength: 30 }),
  model: fc.string({ minLength: 1, maxLength: 50 }),
  status: fc.integer({ min: 100, max: 599 }),
  duration_ms: fc.integer({ min: 0, max: 300000 }),
  is_stream: fc.boolean(),
  error_message: fc.option(fc.string({ minLength: 1, maxLength: 100 }), { nil: undefined }),
  request_body: fc.option(fc.string({ minLength: 1, maxLength: 200 }), { nil: undefined }),
  response_body: fc.option(fc.string({ minLength: 1, maxLength: 200 }), { nil: undefined }),
  token_count: fc.option(fc.integer({ min: 0, max: 10000 }), { nil: undefined }),
});

type StatusRangeFilter = Exclude<StatusFilter, "all">;

const statusFilterArb: fc.Arbitrary<StatusRangeFilter> = fc.constantFrom("2xx", "4xx", "5xx");

// --- Property Tests ---

// Feature: request-logging, Property 9: Status code filter returns only matching entries
describe("Property 9: Status code filter returns only matching entries", () => {
  it("filter function returns only entries whose status code falls within the selected range, and returns all such entries", () => {
    fc.assert(
      fc.property(
        fc.array(logEntryArb, { minLength: 0, maxLength: 50 }),
        statusFilterArb,
        (entries, statusFilter) => {
          const result = filterLogEntries(entries, statusFilter);

          const ranges: Record<StatusRangeFilter, readonly [number, number]> = {
            "2xx": [200, 299],
            "4xx": [400, 499],
            "5xx": [500, 599],
          };
          const [min, max] = ranges[statusFilter];

          // All returned entries must have status in the selected range
          for (const entry of result) {
            expect(entry.status).toBeGreaterThanOrEqual(min);
            expect(entry.status).toBeLessThanOrEqual(max);
          }

          // All entries from input that match the range must be in the result
          const expectedEntries = entries.filter(
            (e) => e.status >= min && e.status <= max
          );
          expect(result.length).toBe(expectedEntries.length);
        }
      ),
      { numRuns: 200 }
    );
  });

  // **Validates: Requirements 4.4**
});

// Feature: request-logging, Property 10: Provider filter returns only matching entries
describe("Property 10: Provider filter returns only matching entries", () => {
  it("provider filter returns exactly those entries whose provider field equals the filter value", () => {
    fc.assert(
      fc.property(
        fc.array(logEntryArb, { minLength: 0, maxLength: 50 }),
        fc.string({ minLength: 1, maxLength: 30 }),
        (entries, providerFilter) => {
          const result = filterLogEntries(entries, undefined, providerFilter);

          // All returned entries must have the matching provider
          for (const entry of result) {
            expect(entry.provider).toBe(providerFilter);
          }

          // All entries from input that match the provider must be in the result
          const expectedEntries = entries.filter(
            (e) => e.provider === providerFilter
          );
          expect(result.length).toBe(expectedEntries.length);
        }
      ),
      { numRuns: 200 }
    );
  });

  // **Validates: Requirements 4.5**
});

// Feature: request-logging, Property 11: Keyword search returns only matching entries
describe("Property 11: Keyword search returns only matching entries", () => {
  it("search function returns only entries where the keyword appears (case-insensitive) in path, model, or error_message", () => {
    fc.assert(
      fc.property(
        fc.array(logEntryArb, { minLength: 0, maxLength: 50 }),
        fc.string({ minLength: 1, maxLength: 20 }),
        (entries, keyword) => {
          const result = filterLogEntries(entries, undefined, undefined, keyword);

          // filterLogEntries skips keyword filtering when keyword.trim() is empty
          if (keyword.trim() === "") {
            expect(result.length).toBe(entries.length);
            return;
          }

          const lowerKeyword = keyword.toLowerCase();

          // All returned entries must contain the keyword in at least one searchable field
          for (const entry of result) {
            const inPath = entry.path.toLowerCase().includes(lowerKeyword);
            const inModel = entry.model.toLowerCase().includes(lowerKeyword);
            const inError = entry.error_message
              ? entry.error_message.toLowerCase().includes(lowerKeyword)
              : false;
            expect(inPath || inModel || inError).toBe(true);
          }

          // All entries from input that match the keyword must be in the result
          const expectedEntries = entries.filter((e) => {
            const inPath = e.path.toLowerCase().includes(lowerKeyword);
            const inModel = e.model.toLowerCase().includes(lowerKeyword);
            const inError = e.error_message
              ? e.error_message.toLowerCase().includes(lowerKeyword)
              : false;
            return inPath || inModel || inError;
          });
          expect(result.length).toBe(expectedEntries.length);
        }
      ),
      { numRuns: 200 }
    );
  });

  // **Validates: Requirements 4.6**
});
