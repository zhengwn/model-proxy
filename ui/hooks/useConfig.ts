import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Config, ServerConfig } from "../types";

const DEFAULT_CONFIG: Config = {
  server: {
    port: 4000,
  },
  providers: [],
  model_routes: [],
};

interface UseConfigReturn {
  config: Config | null;
  loading: boolean;
  error: string | null;
  configPath: string | null;
  isNew: boolean;
  loadConfig: () => Promise<void>;
  saveConfig: (config: Config) => Promise<void>;
  saveServerConfig: (server: ServerConfig) => Promise<void>;
}

export function useConfig(): UseConfigReturn {
  const [config, setConfig] = useState<Config | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [configPath, setConfigPath] = useState<string | null>(null);
  const [isNew, setIsNew] = useState(false);

  const loadConfig = useCallback(async () => {
    setLoading(true);
    setError(null);
    setIsNew(false);
    try {
      const [cfg, path] = await Promise.all([
        invoke<Config>("get_config"),
        invoke<string>("get_config_path"),
      ]);
      setConfig(cfg);
      setConfigPath(path);
    } catch (e) {
      // If config file doesn't exist, use default config and let user save
      const errMsg = typeof e === "string" ? e : String(e);
      if (errMsg.includes("不存在")) {
        setConfig(DEFAULT_CONFIG);
        setIsNew(true);
        try {
          const path = await invoke<string>("get_config_path");
          setConfigPath(path);
        } catch {
          // ignore
        }
      } else {
        setError(errMsg);
      }
    } finally {
      setLoading(false);
    }
  }, []);

  const saveConfig = useCallback(async (newConfig: Config) => {
    await invoke<void>("save_config", { config: newConfig });
    setConfig(newConfig);
  }, []);

  const saveServerConfig = useCallback(async (server: ServerConfig) => {
    await invoke<void>("save_server_config", { server });
    setConfig((current) => ({ ...(current ?? DEFAULT_CONFIG), server }));
    setIsNew(false);
  }, []);

  useEffect(() => {
    loadConfig();
  }, [loadConfig]);

  return {
    config,
    loading,
    error,
    configPath,
    isNew,
    loadConfig,
    saveConfig,
    saveServerConfig,
  };
}
