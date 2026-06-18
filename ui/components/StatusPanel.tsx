import {
  Card,
  Badge,
  Button,
  Statistic,
  Alert,
  Space,
  Row,
  Col,
  Spin,
  message,
  Input,
  InputNumber,
  Typography,
  Tooltip,
  Collapse,
} from "antd";
import {
  PlayCircleOutlined,
  StopOutlined,
  SaveOutlined,
  CopyOutlined,
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
  const [serverAdminApiKey, setServerAdminApiKey] = useState<string>("");
  const [serverDirty, setServerDirty] = useState(false);

  useEffect(() => {
    if (config) {
      setServerHost(config.server.host || "127.0.0.1");
      setServerPort(config.server.port);
      setServerApiKey(config.server.api_key || "");
      setServerAdminApiKey(config.server.admin_api_key || "");
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
          admin_api_key: serverAdminApiKey || undefined,
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
        admin_api_key: serverAdminApiKey || undefined,
      });
      setServerDirty(false);
      message.success(t("status.serverSaved"));
    } catch (err) {
      message.error(t("common.saveFailed", { error: typeof err === "string" ? err : String(err) }));
    }
  };

  const localDisplayHost = !serverHost || serverHost === "0.0.0.0" ? "localhost" : serverHost;

  const handleCopyAddress = () => {
    const addr = `http://${localDisplayHost}:${serverPort}`;
    navigator.clipboard.writeText(addr).then(() => {
      message.success(t("status.copied", { addr }));
    });
  };

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
        <Row gutter={16}>
          <Col span={6}>
            <Statistic title={t("status.currentProvider")} value={activeProvider || "-"} />
          </Col>
          <Col span={6}>
            <Statistic
              title={t("status.listenAddress")}
              value={isRunning ? status?.listen_addr ?? `${serverHost}:${serverPort}` : "-"}
              suffix={
                isRunning && (
                  <Tooltip title={t("status.copyAddress")}>
                    <CopyOutlined
                      style={{ fontSize: 14, cursor: "pointer", color: "#1677ff" }}
                      onClick={handleCopyAddress}
                    />
                  </Tooltip>
                )
              }
            />
          </Col>
          <Col span={6}>
            <Statistic title={t("status.totalRequests")} value={status?.total_requests ?? 0} />
          </Col>
          <Col span={6}>
            <Statistic
              title={t("status.failedRequests")}
              value={status?.failed_requests ?? 0}
              valueStyle={
                (status?.failed_requests ?? 0) > 0
                  ? { color: "#cf1322" }
                  : undefined
              }
            />
          </Col>
        </Row>

        {isRunning && status?.started_at && (
          <div style={{ marginTop: 16 }}>
            <Text type="secondary">
              {t("status.startedAt", { time: formatTimestamp(status.started_at) })}
            </Text>
          </div>
        )}
      </Card>

      {status?.error_message && (
        <Alert
          message={t("status.serviceError")}
          description={status.error_message}
          type="error"
          showIcon
        />
      )}

      {/* Inline server settings */}
      <Collapse
        size="small"
        items={[
          {
            key: "server",
            label: t("status.serverSettings"),
            children: (
              <>
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
                    <Space>
                      <Text>Admin Key:</Text>
                      <Input.Password
                        placeholder={t("status.noAuth")}
                        value={serverAdminApiKey}
                        disabled={isRunning}
                        onChange={(e) => {
                          setServerAdminApiKey(e.target.value);
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
              </>
            ),
          },
        ]}
      />

      {/* Usage hint when running */}
      {isRunning && (
        <Alert
          type="success"
          showIcon={false}
          message={
            <Space direction="vertical" size={2}>
              <Text>
                {t("status.serviceRunningHint", { addr: `${localDisplayHost}:${serverPort}` }).split("{addr}")[0]}
                <Text code copyable>{`http://${localDisplayHost}:${serverPort}`}</Text>
              </Text>
              {serverApiKey && (
                <Text type="secondary" style={{ fontSize: 12 }}>
                  {t("status.apiKeyHint")}
                </Text>
              )}
            </Space>
          }
        />
      )}
    </Space>
  );
}

export default StatusPanel;
