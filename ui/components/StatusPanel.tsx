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

const { Text } = Typography;

function formatTimestamp(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

function StatusPanel() {
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
      message.error(`启动失败: ${errMsg}`);
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
      message.error(`停止失败: ${errMsg}`);
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
      message.success("服务器设置已保存");
    } catch (err) {
      message.error(`保存失败: ${typeof err === "string" ? err : String(err)}`);
    }
  };

  const localDisplayHost = !serverHost || serverHost === "0.0.0.0" ? "localhost" : serverHost;

  const handleCopyAddress = () => {
    const addr = `http://${localDisplayHost}:${serverPort}`;
    navigator.clipboard.writeText(addr).then(() => {
      message.success("已复制: " + addr);
    });
  };

  if (loading && !status) {
    return <Spin tip="加载中..." />;
  }

  const isRunning = status?.running ?? false;
  const hasProviders = providers.length > 0;
  const canStart = hasProviders && !isRunning;

  return (
    <Space direction="vertical" size="middle" style={{ width: "100%" }}>
      {/* First-launch guide */}
      {isNew && (
        <Alert
          message="首次使用"
          description="请先在「Provider 管理」页面添加至少一个 Provider，然后回到此页面启动服务。"
          type="info"
          showIcon
        />
      )}

      {!hasProviders && !isNew && (
        <Alert
          message="尚未配置 Provider"
          description="请先在「Provider 管理」页面添加至少一个 Provider 才能启动服务。"
          type="warning"
          showIcon
        />
      )}

      {/* Main status card */}
      <Card
        title={
          <Space>
            <span>服务状态</span>
            <Badge
              status={isRunning ? "success" : "error"}
              text={isRunning ? "运行中" : "已停止"}
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
              启动
            </Button>
            <Button
              danger
              icon={<StopOutlined />}
              onClick={handleStop}
              disabled={!isRunning}
              loading={actionLoading && isRunning}
            >
              停止
            </Button>
          </Space>
        }
      >
        <Row gutter={16}>
          <Col span={6}>
            <Statistic title="当前 Provider" value={activeProvider || "-"} />
          </Col>
          <Col span={6}>
            <Statistic
              title="监听地址"
              value={isRunning ? status?.listen_addr ?? `${serverHost}:${serverPort}` : "-"}
              suffix={
                isRunning && (
                  <Tooltip title="复制连接地址">
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
            <Statistic title="总请求数" value={status?.total_requests ?? 0} />
          </Col>
          <Col span={6}>
            <Statistic
              title="失败请求数"
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
              启动于 {formatTimestamp(status.started_at)}
            </Text>
          </div>
        )}
      </Card>

      {status?.error_message && (
        <Alert
          message="服务错误"
          description={status.error_message}
          type="error"
          showIcon
        />
      )}

      {/* Inline server settings */}
      <Card title="服务器设置" size="small">
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
              <Text>端口:</Text>
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
                placeholder="留空则不鉴权"
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
                placeholder="留空则不鉴权"
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
                保存
              </Button>
            )}
          </Col>
        </Row>
        {configPath && (
          <Text
            type="secondary"
            style={{ display: "block", marginTop: 8, fontSize: 12 }}
          >
            配置文件: {configPath}
          </Text>
        )}
      </Card>

      {/* Usage hint when running */}
      {isRunning && (
        <Alert
          type="success"
          showIcon={false}
          message={
            <Space direction="vertical" size={2}>
              <Text>
                服务已启动，将 IDE 的 API Base URL 设置为{" "}
                <Text code copyable>{`http://${localDisplayHost}:${serverPort}`}</Text>
              </Text>
              {serverApiKey && (
                <Text type="secondary" style={{ fontSize: 12 }}>
                  API Key 设置为你上方配置的值
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
