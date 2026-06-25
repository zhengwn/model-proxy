import {
  Card,
  Badge,
  Button,
  Alert,
  Space,
  Row,
  Col,
  Spin,
  message,
  Input,
  InputNumber,
  Typography,
} from "antd";
import {
  PlayCircleOutlined,
  StopOutlined,
  SaveOutlined,
} from "@ant-design/icons";
import { useState, useEffect } from "react";
import { useServiceStatus } from "../hooks/useServiceStatus";
import { useProviders } from "../hooks/useProviders";
import { useConfig } from "../hooks/useConfig";
import { useLocale } from "../i18n";

const { Text } = Typography;

function formatTimestamp(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

function StatusPanel() {
  const { t } = useLocale();
  const { status, loading, startService, stopService } = useServiceStatus();
  const { providers, activeProvider, loadProviders } = useProviders();

  // Refresh providers when service status changes (start/stop) or periodically as fallback
  useEffect(() => {
    loadProviders();
  }, [loadProviders, status?.running]);

  useEffect(() => {
    const interval = setInterval(loadProviders, 15000);
    return () => clearInterval(interval);
  }, [loadProviders]);
  const { config, configPath, isNew, saveServerConfig } = useConfig();
  const [actionLoading, setActionLoading] = useState(false);
  const [serverHost, setServerHost] = useState<string>("127.0.0.1");
  const [serverPort, setServerPort] = useState<number>(4000);
  const [serverApiKey, setServerApiKey] = useState<string>("");
  const [serverDirty, setServerDirty] = useState(false);

  useEffect(() => {
    if (config) {
      setServerHost(config.server.host || "127.0.0.1");
      setServerPort(config.server.port);
      setServerApiKey(config.server.api_key || "");
    }
  }, [config]);

  const handleStart = async () => {
    setActionLoading(true);
    try {
      if (serverDirty && config) {
        await saveServerConfig({
          ...config.server,
          host: serverHost || "127.0.0.1",
          port: serverPort,
          api_key: serverApiKey || undefined,
        });
        setServerDirty(false);
      }
      await startService();
    } catch (err) {
      const errMsg = typeof err === "string" ? err : String(err);
      message.error(t("status.startFailed", { error: errMsg }));
    } finally {
      setActionLoading(false);
    }
  };

  const handleStop = async () => {
    setActionLoading(true);
    try {
      await stopService();
    } catch (err) {
      const errMsg = typeof err === "string" ? err : String(err);
      message.error(t("status.stopFailed", { error: errMsg }));
    } finally {
      setActionLoading(false);
    }
  };

  const handleSaveServer = async () => {
    if (!config) return;
    try {
      await saveServerConfig({
        ...config.server,
        host: serverHost || "127.0.0.1",
        port: serverPort,
        api_key: serverApiKey || undefined,
      });
      setServerDirty(false);
      message.success(t("status.serverSaved"));
    } catch (err) {
      message.error(t("common.saveFailed", { error: typeof err === "string" ? err : String(err) }));
    }
  };

  const localDisplayHost = !serverHost || serverHost === "0.0.0.0" ? "localhost" : serverHost;

  if (loading && !status) {
    return <Spin tip={t("common.loading")} />;
  }

  const isRunning = status?.running ?? false;
  const hasProviders = providers.length > 0;
  const canStart = hasProviders && !isRunning;

  return (
    <Space direction="vertical" size="middle" style={{ width: "100%" }}>
      {/* First-launch guide */}
      {isNew && (
        <Alert
          message={t("status.firstUse")}
          description={t("status.firstUseDesc")}
          type="info"
          showIcon
        />
      )}

      {!hasProviders && !isNew && (
        <Alert
          message={t("status.noProviders")}
          description={t("status.noProvidersDesc")}
          type="warning"
          showIcon
        />
      )}

      {/* Main status card */}
      <Card
        title={
          <Space>
            <span>{t("status.serviceStatus")}</span>
            <Badge
              status={isRunning ? "success" : "error"}
              text={isRunning ? t("status.running") : t("status.stopped")}
            />
          </Space>
        }
        extra={
          <Space>
            <Button
              type="primary"
              icon={<PlayCircleOutlined />}
              onClick={handleStart}
              disabled={!canStart}
              loading={actionLoading && !isRunning}
            >
              {t("status.start")}
            </Button>
            <Button
              danger
              icon={<StopOutlined />}
              onClick={handleStop}
              disabled={!isRunning}
              loading={actionLoading && isRunning}
            >
              {t("status.stop")}
            </Button>
          </Space>
        }
      >
        <div style={{ height: 106, display: "flex", flexDirection: "column", justifyContent: "center" }}>
          {isRunning && status?.started_at ? (
            <Space direction="vertical" style={{ width: "100%" }}>
              <Text type="secondary">
                {t("status.startedAt", { time: formatTimestamp(status.started_at) })}
              </Text>
              <Alert
                type="success"
                showIcon={false}
                message={
                  <Space direction="vertical" size={2}>
                    <Text>
                      {t("status.serviceRunningHint", { addr: "|||ADDR|||" }).split("|||ADDR|||")[0]}
                      <Text code copyable>{`http://${localDisplayHost}:${serverPort}`}</Text>
                      {t("status.serviceRunningHint", { addr: "|||ADDR|||" }).split("|||ADDR|||")[1]}
                    </Text>
                    {serverApiKey && (
                      <Text type="secondary" style={{ fontSize: 12 }}>
                        {t("status.apiKeyHint")}
                      </Text>
                    )}
                  </Space>
                }
                style={{ marginTop: 8 }}
              />
            </Space>
          ) : (
            <div style={{ textAlign: "center", color: "var(--ant-color-text-secondary)" }}>
              <StopOutlined style={{ fontSize: 24, marginBottom: 8, opacity: 0.4 }} />
              <br />
              <Text type="secondary" style={{ fontSize: 13 }}>
                {t("status.stoppedHint")}
              </Text>
            </div>
          )}
        </div>
      </Card>

      {/* Grid of Metric Cards */}
      <Row gutter={[12, 12]} align="stretch">
        <Col span={8}>
          <Card style={{ height: "100%" }} bodyStyle={{ padding: "12px 16px" }}>
            <Text type="secondary" style={{ fontSize: 13, display: "block", marginBottom: 6 }}>
              {t("status.currentProvider")}
            </Text>
            <div style={{ fontSize: 16, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", fontWeight: 500 }}>
              {activeProvider || "-"}
            </div>
          </Card>
        </Col>
        
        <Col span={8}>
          <Card style={{ height: "100%" }} bodyStyle={{ padding: "12px 16px" }}>
            <Text type="secondary" style={{ fontSize: 13, display: "block", marginBottom: 6 }}>
              {t("status.listenAddress")}
            </Text>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
              <div style={{ fontSize: 16, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", fontWeight: 500 }}>
                {isRunning ? status?.listen_addr ?? `${serverHost}:${serverPort}` : "-"}
              </div>
            </div>
          </Card>
        </Col>

        <Col span={8}>
          <Card style={{ height: "100%" }} bodyStyle={{ padding: "12px 16px" }}>
            <Text type="secondary" style={{ fontSize: 13, display: "block", marginBottom: 6 }}>
              {t("status.requestStats")}
            </Text>
            <div style={{ display: "flex", alignItems: "baseline", gap: 8 }}>
              <div style={{ fontSize: 16, fontWeight: 500 }}>
                {status?.total_requests ?? 0}
              </div>
              <div style={{ color: "var(--ant-color-text-secondary)", fontSize: 14 }}>/</div>
              <div style={{ 
                fontSize: 16, 
                fontWeight: 500,
                color: (status?.failed_requests ?? 0) > 0 ? "#cf1322" : undefined
              }}>
                {status?.failed_requests ?? 0}
              </div>
            </div>
          </Card>
        </Col>
      </Row>

      {status?.error_message && (
        <Alert
          message={t("status.serviceError")}
          description={status.error_message}
          type="error"
          showIcon
        />
      )}

      {/* Server settings card */}
      <Card title={t("status.serverSettings")}>
        <Row gutter={[16, 12]} align="middle">
                  <Col>
                    <Space>
                      <Text>Host:</Text>
                      <Input
                        placeholder="127.0.0.1"
                        value={serverHost}
                        disabled={isRunning}
                        onChange={(e) => {
                          setServerHost(e.target.value);
                          setServerDirty(true);
                        }}
                        style={{ width: 140 }}
                      />
                    </Space>
                  </Col>
                  <Col>
                    <Space>
                      <Text>{t("status.port")}</Text>
                      <InputNumber
                        min={1}
                        max={65535}
                        value={serverPort}
                        disabled={isRunning}
                        onChange={(v) => {
                          setServerPort(v ?? 4000);
                          setServerDirty(true);
                        }}
                        style={{ width: 100 }}
                      />
                    </Space>
                  </Col>
                  <Col>
                    <Space>
                      <Text>Client Key:</Text>
                      <Input.Password
                        placeholder={t("status.noAuth")}
                        value={serverApiKey}
                        disabled={isRunning}
                        onChange={(e) => {
                          setServerApiKey(e.target.value);
                          setServerDirty(true);
                        }}
                        style={{ width: 200 }}
                      />
                    </Space>
                  </Col>
                  <Col>
                    {serverDirty && !isRunning && (
                      <Button
                        icon={<SaveOutlined />}
                        size="small"
                        onClick={handleSaveServer}
                      >
                        {t("common.save")}
                      </Button>
                    )}
                  </Col>
                </Row>
        {configPath && (
          <Text
            type="secondary"
            style={{ display: "block", marginTop: 8, fontSize: 12 }}
          >
            {t("status.configFile", { path: configPath })}
          </Text>
        )}
      </Card>

    </Space>
  );
}

export default StatusPanel;
