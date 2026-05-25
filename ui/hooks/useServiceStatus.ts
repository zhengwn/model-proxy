import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ServiceStatus } from "../types";

interface UseServiceStatusReturn {
  status: ServiceStatus | null;
  loading: boolean;
  startService: () => Promise<void>;
  stopService: () => Promise<void>;
}

export function useServiceStatus(): UseServiceStatusReturn {
  const [status, setStatus] = useState<ServiceStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const throttleRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pendingRef = useRef(false);

  const fetchStatus = useCallback(async () => {
    try {
      const result = await invoke<ServiceStatus>("get_service_status");
      setStatus(result);
    } catch (err) {
      console.error("Failed to get service status:", err);
    } finally {
      setLoading(false);
    }
  }, []);

  // Throttled version: at most once per second
  const throttledFetchStatus = useCallback(() => {
    if (throttleRef.current) {
      // Already scheduled, mark as pending
      pendingRef.current = true;
      return;
    }
    fetchStatus();
    throttleRef.current = setTimeout(() => {
      throttleRef.current = null;
      if (pendingRef.current) {
        pendingRef.current = false;
        fetchStatus();
      }
    }, 1000);
  }, [fetchStatus]);

  const startService = useCallback(async () => {
    await invoke("start_service");
    await fetchStatus();
  }, [fetchStatus]);

  const stopService = useCallback(async () => {
    await invoke("stop_service");
    await fetchStatus();
  }, [fetchStatus]);

  useEffect(() => {
    fetchStatus();
    // Poll every 5 seconds as a fallback; primary updates come from proxy-log events
    intervalRef.current = setInterval(fetchStatus, 5000);

    // Listen for proxy-log events to trigger throttled status refresh
    // (each log entry means a request was processed, so counters changed)
    const unlisten = listen("proxy-log", () => {
      throttledFetchStatus();
    });

    return () => {
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
      }
      if (throttleRef.current) {
        clearTimeout(throttleRef.current);
      }
      unlisten.then((fn) => fn());
    };
  }, [fetchStatus, throttledFetchStatus]);

  return { status, loading, startService, stopService };
}
