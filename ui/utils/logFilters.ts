import type { LogEntry } from "../types";

export type StatusFilter = "all" | "2xx" | "4xx" | "5xx";

/**
 * Filter log entries by status code range, provider name, and keyword search.
 * All filters are independently applicable and composable (applied together via AND logic).
 */
export function filterLogEntries(
  entries: LogEntry[],
  statusFilter?: StatusFilter,
  providerFilter?: string,
  keyword?: string
): LogEntry[] {
  let result = entries;

  if (statusFilter && statusFilter !== "all") {
    result = result.filter((entry) => matchesStatusFilter(entry.status, statusFilter));
  }

  if (providerFilter) {
    result = result.filter((entry) => entry.provider === providerFilter);
  }

  if (keyword && keyword.trim() !== "") {
    const lowerKeyword = keyword.toLowerCase();
    result = result.filter((entry) => matchesKeyword(entry, lowerKeyword));
  }

  return result;
}

/**
 * Check if a status code matches the given status filter range.
 */
export function matchesStatusFilter(status: number, filter: StatusFilter): boolean {
  switch (filter) {
    case "2xx":
      return status >= 200 && status <= 299;
    case "4xx":
      return status >= 400 && status <= 499;
    case "5xx":
      return status >= 500 && status <= 599;
    case "all":
      return true;
    default:
      return true;
  }
}

/**
 * Check if a log entry matches the keyword (case-insensitive) in path, model, or error_message.
 */
export function matchesKeyword(entry: LogEntry, lowerKeyword: string): boolean {
  if (entry.path.toLowerCase().includes(lowerKeyword)) {
    return true;
  }
  if (entry.model.toLowerCase().includes(lowerKeyword)) {
    return true;
  }
  if (entry.error_message && entry.error_message.toLowerCase().includes(lowerKeyword)) {
    return true;
  }
  return false;
}

/**
 * Extract unique provider names from log entries for populating the provider filter dropdown.
 */
export function getUniqueProviders(entries: LogEntry[]): string[] {
  const providers = new Set<string>();
  for (const entry of entries) {
    if (entry.provider) {
      providers.add(entry.provider);
    }
  }
  return Array.from(providers).sort();
}
