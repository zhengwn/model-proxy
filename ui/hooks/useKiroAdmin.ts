import { useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  KiroCredential,
  KiroBatchResult,
  KiroTestResult,
  KiroEndpointHealth,
  KiroThinkingConfig,
  KiroSettings,
} from "../types";

export function useKiroAdmin() {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const call = useCallback(
    async <T>(fn: () => Promise<T>): Promise<T> => {
      setLoading(true);
      setError(null);
      try {
        return await fn();
      } catch (e: unknown) {
        const msg = typeof e === "string" ? e : String(e);
        setError(msg);
        throw e;
      } finally {
        setLoading(false);
      }
    },
    []
  );

  const listCredentials = useCallback(
    () => call(() => invoke<KiroCredential[]>("kiro_list_credentials")),
    [call]
  );

  const addCredential = useCallback(
    (refreshToken: string, authMethod?: string, region?: string, priority?: number) =>
      call(() =>
        invoke("kiro_add_credential", {
          refreshToken,
          authMethod: authMethod ?? null,
          region: region ?? null,
          priority: priority ?? null,
        })
      ),
    [call]
  );

  const deleteCredential = useCallback(
    (id: string) => call(() => invoke("kiro_delete_credential", { id })),
    [call]
  );

  const setDisabled = useCallback(
    (id: string, disabled: boolean) =>
      call(() => invoke("kiro_set_credential_disabled", { id, disabled })),
    [call]
  );

  const batchCredentials = useCallback(
    (ids: string[], action: string) =>
      call(() => invoke<KiroBatchResult>("kiro_batch_credentials", { ids, action })),
    [call]
  );

  const testCredential = useCallback(
    (id: string) => call(() => invoke<KiroTestResult>("kiro_test_credential", { id })),
    [call]
  );

  const getCredentialFull = useCallback(
    (id: string) => call(() => invoke("kiro_get_credential_full", { id })),
    [call]
  );

  const refreshCredential = useCallback(
    (id: string) => call(() => invoke("kiro_refresh_credential", { id })),
    [call]
  );

  const resetCredential = useCallback(
    (id: string) => call(() => invoke("kiro_reset_credential", { id })),
    [call]
  );

  const getEndpointHealth = useCallback(
    () => call(() => invoke<KiroEndpointHealth>("kiro_get_endpoint_health")),
    [call]
  );

  const getThinking = useCallback(
    () => call(() => invoke<KiroThinkingConfig>("kiro_get_thinking")),
    [call]
  );

  const setThinking = useCallback(
    (mode: string) => call(() => invoke("kiro_set_thinking", { mode })),
    [call]
  );

  const getSettings = useCallback(
    () => call(() => invoke<KiroSettings>("kiro_get_settings")),
    [call]
  );

  const setSettings = useCallback(
    (preferredEndpoint?: string, endpointFallback?: boolean) =>
      call(() =>
        invoke("kiro_set_settings", {
          preferredEndpoint: preferredEndpoint ?? null,
          endpointFallback: endpointFallback ?? null,
        })
      ),
    [call]
  );

  const startIamSso = useCallback(
    (startUrl: string, region: string) =>
      call(() => invoke("kiro_start_iam_sso", { startUrl, region })),
    [call]
  );

  const completeIamSso = useCallback(
    (sessionId: string, callbackUrl: string) =>
      call(() => invoke("kiro_complete_iam_sso", { sessionId, callbackUrl })),
    [call]
  );

  const importSsoTokens = useCallback(
    (tokens: string, region?: string) =>
      call(() => invoke("kiro_import_sso_tokens", { tokens, region: region ?? null })),
    [call]
  );

  const getLbConfig = useCallback(
    () => call(() => invoke("kiro_get_lb_config")),
    [call]
  );

  const setLbConfig = useCallback(
    (mode: string) => call(() => invoke("kiro_set_lb_config", { mode })),
    [call]
  );

  return {
    loading,
    error,
    setError,
    listCredentials,
    addCredential,
    deleteCredential,
    setDisabled,
    batchCredentials,
    testCredential,
    getCredentialFull,
    refreshCredential,
    resetCredential,
    getEndpointHealth,
    getThinking,
    setThinking,
    getSettings,
    setSettings,
    startIamSso,
    completeIamSso,
    importSsoTokens,
    getLbConfig,
    setLbConfig,
  };
}
