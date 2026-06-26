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
  Progress,
  Tooltip,
  Tag,
} from "antd";
import {
  PlayCircleOutlined,
  StopOutlined,
  SaveOutlined,
  CloudServerOutlined,
  GlobalOutlined,
  ThunderboltOutlined,
  PlusOutlined,
  LockOutlined,
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

interface StatusPanelProps {
  onGoAddProvider?: () => void;
}

function StatusPanel({ onGoAddProvider }: StatusPanelProps = {}) {
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
    <Space direction="vertical" size={10} style={{ width: "100%" }}>
      {/* First-launch guide */}
      {isNew && (
        <Alert
          title={t("status.firstUse")}
          description={t("status.firstUseDesc")}
          type="info"
          showIcon
          action={
            !hasProviders &&
            onGoAddProvider && (
              <Button type="primary" size="small" icon={<PlusOutlined />} onClick={onGoAddProvider}>
                {t("status.goAddProvider")}
              </Button>
            )
          }
        />
      )}

      {!hasProviders && !isNew && (
        <Alert
          title={t("status.noProviders")}
          description={t("status.noProvidersDesc")}
          type="warning"
          showIcon
          action={
            onGoAddProvider && (
              <Button type="primary" size="small" icon={<PlusOutlined />} onClick={onGoAddProvider}>
                {t("status.goAddProvider")}
              </Button>
            )
          }
        />
      )}

      {/* Main status card */}
      <Card
        styles={{ body: { padding: "18px 24px" } }}
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
            <Tooltip title={!hasProviders && !isRunning ? t("status.startDisabledTip") : ""}>
              <Button
                type="primary"
                icon={<PlayCircleOutlined />}
                onClick={handleStart}
                disabled={!canStart}
                loading={actionLoading && !isRunning}
              >
                {t("status.start")}
              </Button>
            </Tooltip>
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
        <div style={{ minHeight: 108, display: "flex", flexDirection: "column", justifyContent: "center" }}>
          {isRunning && status?.started_at ? (
            <Space direction="vertical" style={{ width: "100%" }}>
              <Text type="secondary">
                {t("status.startedAt", { time: formatTimestamp(status.started_at) })}
              </Text>
              <Alert
                type="success"
                showIcon={false}
                title={
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
        <Col xs={24} sm={8}>
          <Card className="metric-card" style={{ height: "100%" }} bodyStyle={{ padding: 16 }}>
            <div className="metric-head">
              <span className="metric-icon" style={{ background: "rgba(79,70,229,0.12)", color: "#4f46e5" }}>
                <CloudServerOutlined />
              </span>
              <Text type="secondary" style={{ fontSize: 13 }}>
                {t("status.currentProvider")}
              </Text>
            </div>
            <div className="metric-value" style={{ whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
              {activeProvider || "-"}
            </div>
          </Card>
        </Col>

        <Col xs={24} sm={8}>
          <Card className="metric-card" style={{ height: "100%" }} bodyStyle={{ padding: 16 }}>
            <div className="metric-head">
              <span className="metric-icon" style={{ background: "rgba(8,145,178,0.12)", color: "#0891b2" }}>
                <GlobalOutlined />
              </span>
              <Text type="secondary" style={{ fontSize: 13 }}>
                {t("status.listenAddress")}
              </Text>
            </div>
            <div className="metric-value" style={{ whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", fontVariantNumeric: "tabular-nums" }}>
              {isRunning ? status?.listen_addr ?? `${serverHost}:${serverPort}` : "-"}
            </div>
          </Card>
        </Col>

        <Col xs={24} sm={8}>
          <Card className="metric-card" style={{ height: "100%" }} bodyStyle={{ padding: 16 }}>
            <div className="metric-head">
              <span className="metric-icon" style={{ background: "rgba(217,119,6,0.12)", color: "#d97706" }}>
                <ThunderboltOutlined />
              </span>
              <Text type="secondary" style={{ fontSize: 13, whiteSpace: "nowrap" }}>
                {t("status.requestStats")}
              </Text>
            </div>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 12 }}>
              <div className="metric-value" style={{ display: "flex", alignItems: "baseline", gap: 6, fontVariantNumeric: "tabular-nums" }}>
                <span>{status?.total_requests ?? 0}</span>
                <span style={{ fontSize: 13, fontWeight: 400, color: "var(--ant-color-text-tertiary)" }}>/</span>
                <span style={{ color: (status?.failed_requests ?? 0) > 0 ? "#dc2626" : undefined }}>
                  {status?.failed_requests ?? 0}
                </span>
              </div>
              {(() => {
                const total = status?.total_requests ?? 0;
                const failed = status?.failed_requests ?? 0;
                const rate = total > 0 ? Math.round(((total - failed) / total) * 100) : 100;
                return (
                  <Progress
                    type="circle"
                    size={44}
                    percent={rate}
                    strokeColor={failed > 0 ? "#d97706" : "#16a34a"}
                    strokeWidth={9}
                    format={(p) => <span style={{ fontSize: 11, fontWeight: 600 }}>{p}%</span>}
                  />
                );
              })()}
            </div>
          </Card>
        </Col>
      </Row>

      {status?.error_message && (
        <Alert
          title={t("status.serviceError")}
          description={status.error_message}
          type="error"
          showIcon
        />
      )}

      {/* Server settings card */}
      <Card
        styles={{ body: { padding: "18px 24px" } }}
        title={
          <Space>
            <span>{t("status.serverSettings")}</span>
            <Tooltip title={t("status.restartTagTip")}>
              <Tag icon={<LockOutlined />} color="default" style={{ cursor: "help" }}>
                {t("status.restartTag")}
              </Tag>
            </Tooltip>
          </Space>
        }
      >
        {isRunning && (
          <Alert
            type="info"
            showIcon
            icon={<LockOutlined />}
            title={t("status.serverLockedHint")}
            style={{ marginBottom: 8 }}
          />
        )}
        <Row gutter={[12, 12]} align="middle" wrap>
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
                        style={{ width: 120 }}
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
                        style={{ width: 80 }}
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
                        style={{ width: 160 }}
                      />
                    </Space>
                  </Col>
                  <Col>
                    {serverDirty && !isRunning && (
                      <Button
                        type="primary"
                        icon={<SaveOutlined />}
                        size="small"
                        onClick={handleSaveServer}
                        title={t("common.save")}
                      />
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
