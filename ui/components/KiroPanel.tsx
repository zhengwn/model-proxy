import { useState, useEffect, useCallback } from "react";
import {
  Card,
  Table,
  Button,
  Space,
  Tag,
  Modal,
  Form,
  Input,
  Select,
  Switch,
  Tabs,
  Statistic,
  Row,
  Col,
  Progress,
  message,
  Popconfirm,
  Tooltip,
  Alert,
  Typography,
} from "antd";
import {
  PlusOutlined,
  DeleteOutlined,
  ReloadOutlined,
  ThunderboltOutlined,
  CheckCircleOutlined,
  CloseCircleOutlined,
  ExperimentOutlined,
  SafetyOutlined,
  ApiOutlined,
} from "@ant-design/icons";
import { useKiroAdmin } from "../hooks/useKiroAdmin";
import type { KiroCredential, KiroEndpointHealth, KiroThinkingConfig, KiroSettings } from "../types";

const { Text } = Typography;

export default function KiroPanel() {
  return (
    <Tabs
      items={[
        { key: "creds", label: "账户管理", children: <CredentialManager /> },
        { key: "endpoints", label: "端点健康", children: <EndpointDashboard /> },
        { key: "settings", label: "配置", children: <SettingsPanel /> },
        { key: "auth", label: "SSO 登录", children: <AuthFlows /> },
      ]}
    />
  );
}

// ---- Credential Manager ----

function CredentialManager() {
  const kiro = useKiroAdmin();
  const [creds, setCreds] = useState<KiroCredential[]>([]);
  const [addOpen, setAddOpen] = useState(false);
  const [detailId, setDetailId] = useState<string | null>(null);
  const [detail, setDetail] = useState<unknown>(null);
  const [selectedIds, setSelectedIds] = useState<string[]>([]);

  const refresh = useCallback(async () => {
    try {
      const data = await kiro.listCredentials();
      const list = Array.isArray(data) ? data : (data as Record<string, unknown>)?.credentials ?? [];
      setCreds(list as KiroCredential[]);
    } catch {
      // error shown by hook
    }
  }, [kiro]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const handleTest = async (id: string) => {
    try {
      const result = (await kiro.testCredential(id)) as unknown as Record<string, unknown>;
      if (result?.success) {
        message.success(`测试通过 (${result.latency_ms}ms)`);
      } else {
        message.error(`测试失败: ${result?.error ?? "未知错误"}`);
      }
    } catch {
      // handled
    }
  };

  const handleDelete = async (id: string) => {
    try {
      await kiro.deleteCredential(id);
      message.success("已删除");
      refresh();
    } catch {
      // handled
    }
  };

  const handleToggle = async (id: string, disabled: boolean) => {
    try {
      await kiro.setDisabled(id, disabled);
      message.success(disabled ? "已禁用" : "已启用");
      refresh();
    } catch {
      // handled
    }
  };

  const handleBatch = async (action: string) => {
    if (selectedIds.length === 0) {
      message.warning("请先选择账户");
      return;
    }
    try {
      const result = (await kiro.batchCredentials(selectedIds, action)) as unknown as Record<string, unknown>;
      message.success(`批量操作完成: ${(result?.results as string[])?.length ?? 0} 项`);
      setSelectedIds([]);
      refresh();
    } catch {
      // handled
    }
  };

  const handleDetail = async (id: string) => {
    try {
      const data = await kiro.getCredentialFull(id);
      setDetail(data);
      setDetailId(id);
    } catch {
      // handled
    }
  };

  const columns = [
    {
      title: "ID",
      dataIndex: "id",
      key: "id",
      render: (id: string) => (
        <Button type="link" size="small" onClick={() => handleDetail(id)}>
          {id}
        </Button>
      ),
    },
    {
      title: "状态",
      key: "status",
      render: (_: unknown, record: KiroCredential) => (
        <Space>
          {record.disabled ? (
            <Tag color="red">已禁用</Tag>
          ) : record.is_available ? (
            <Tag color="green">可用</Tag>
          ) : (
            <Tag color="orange">不可用</Tag>
          )}
          {record.is_current && <Tag color="blue">当前</Tag>}
        </Space>
      ),
    },
    {
      title: "区域",
      dataIndex: "region",
      key: "region",
      width: 100,
    },
    {
      title: "优先级",
      dataIndex: "priority",
      key: "priority",
      width: 70,
      sorter: (a: KiroCredential, b: KiroCredential) => a.priority - b.priority,
    },
    {
      title: "健康分",
      dataIndex: "health_score",
      key: "health_score",
      width: 100,
      render: (score: number) => (
        <Progress
          percent={score}
          size="small"
          status={score > 60 ? "normal" : score > 30 ? "active" : "exception"}
          format={(p) => `${p}`}
        />
      ),
    },
    {
      title: "请求",
      key: "requests",
      width: 100,
      render: (_: unknown, r: KiroCredential) => (
        <Tooltip title={`成功: ${r.successful_requests} 失败: ${r.failed_requests}`}>
          <Text>{r.total_requests}</Text>
        </Tooltip>
      ),
    },
    {
      title: "操作",
      key: "actions",
      width: 250,
      render: (_: unknown, record: KiroCredential) => (
        <Space size="small">
          <Button size="small" icon={<ExperimentOutlined />} onClick={() => handleTest(record.id)}>
            测试
          </Button>
          <Button
            size="small"
            icon={<ReloadOutlined />}
            onClick={async () => {
              try {
                await kiro.refreshCredential(record.id);
                message.success("已刷新");
                refresh();
              } catch {}
            }}
          >
            刷新
          </Button>
          <Button
            size="small"
            onClick={() => handleToggle(record.id, !record.disabled)}
          >
            {record.disabled ? "启用" : "禁用"}
          </Button>
          <Popconfirm title="确认删除?" onConfirm={() => handleDelete(record.id)}>
            <Button size="small" danger icon={<DeleteOutlined />} />
          </Popconfirm>
        </Space>
      ),
    },
  ];

  return (
    <Card
      title="Kiro 账户管理"
      extra={
        <Space>
          {selectedIds.length > 0 && (
            <>
              <Button onClick={() => handleBatch("enable")}>批量启用</Button>
              <Button onClick={() => handleBatch("disable")}>批量禁用</Button>
              <Button onClick={() => handleBatch("refresh")}>批量刷新</Button>
            </>
          )}
          <Button icon={<ReloadOutlined />} onClick={refresh}>
            刷新
          </Button>
          <Button type="primary" icon={<PlusOutlined />} onClick={() => setAddOpen(true)}>
            添加账户
          </Button>
        </Space>
      }
    >
      {kiro.error && (
        <Alert
          type="error"
          message={kiro.error}
          closable
          onClose={() => kiro.setError(null)}
          style={{ marginBottom: 16 }}
          description="请确认代理服务已启动，且已配置 admin_api_key"
        />
      )}

      <Table
        dataSource={creds}
        columns={columns}
        rowKey="id"
        size="small"
        pagination={false}
        rowSelection={{
          selectedRowKeys: selectedIds,
          onChange: (keys) => setSelectedKeys(keys as string[]),
        }}
        loading={kiro.loading}
      />

      <AddCredentialModal
        open={addOpen}
        onClose={() => setAddOpen(false)}
        onAdded={() => {
          setAddOpen(false);
          refresh();
        }}
        kiro={kiro}
      />

      <Modal
        title={`账户详情: ${detailId}`}
        open={!!detailId}
        onCancel={() => {
          setDetailId(null);
          setDetail(null);
        }}
        footer={null}
        width={600}
      >
        <pre style={{ maxHeight: 400, overflow: "auto", fontSize: 12 }}>
          {detail ? JSON.stringify(detail, null, 2) : "加载中..."}
        </pre>
      </Modal>
    </Card>
  );
}

// Helper for row selection
function setSelectedKeys(_keys: string[]) {
  // This is handled by the Table component's onChange
}

function AddCredentialModal({
  open,
  onClose,
  onAdded,
  kiro,
}: {
  open: boolean;
  onClose: () => void;
  onAdded: () => void;
  kiro: ReturnType<typeof useKiroAdmin>;
}) {
  const [form] = Form.useForm();

  const handleOk = async () => {
    try {
      const values = await form.validateFields();
      await kiro.addCredential(
        values.refresh_token,
        values.auth_method,
        values.region,
        values.priority
      );
      message.success("账户已添加");
      form.resetFields();
      onAdded();
    } catch {
      // validation or API error
    }
  };

  return (
    <Modal
      title="添加 Kiro 账户"
      open={open}
      onCancel={onClose}
      onOk={handleOk}
      confirmLoading={kiro.loading}
    >
      <Form form={form} layout="vertical">
        <Form.Item
          name="refresh_token"
          label="Refresh Token"
          rules={[{ required: true, message: "请输入 refresh token" }]}
        >
          <Input.TextArea rows={3} placeholder="粘贴 refresh token" />
        </Form.Item>
        <Form.Item name="auth_method" label="认证方式" initialValue="social">
          <Select
            options={[
              { label: "Social (Kiro Desktop)", value: "social" },
              { label: "IdC (AWS SSO)", value: "idc" },
              { label: "API Key", value: "api_key" },
            ]}
          />
        </Form.Item>
        <Form.Item name="region" label="区域" initialValue="us-east-1">
          <Select
            options={[
              { label: "us-east-1", value: "us-east-1" },
              { label: "us-west-2", value: "us-west-2" },
              { label: "eu-west-1", value: "eu-west-1" },
              { label: "ap-northeast-1", value: "ap-northeast-1" },
            ]}
          />
        </Form.Item>
        <Form.Item name="priority" label="优先级" initialValue={0}>
          <Input type="number" placeholder="0 = 最高" />
        </Form.Item>
      </Form>
    </Modal>
  );
}

// ---- Endpoint Dashboard ----

function EndpointDashboard() {
  const kiro = useKiroAdmin();
  const [health, setHealth] = useState<KiroEndpointHealth | null>(null);

  const refresh = useCallback(async () => {
    try {
      const data = await kiro.getEndpointHealth();
      setHealth(data);
    } catch {
      // handled
    }
  }, [kiro]);

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 10000);
    return () => clearInterval(interval);
  }, [refresh]);

  return (
    <Card
      title="端点健康状态"
      extra={
        <Button icon={<ReloadOutlined />} onClick={refresh}>
          刷新
        </Button>
      }
    >
      {kiro.error && (
        <Alert type="error" message={kiro.error} closable onClose={() => kiro.setError(null)} style={{ marginBottom: 16 }} />
      )}

      <Row gutter={[16, 16]}>
        {health?.endpoints?.map((ep) => (
          <Col span={8} key={ep.endpoint}>
            <Card size="small">
              <Statistic
                title={ep.endpoint}
                value={ep.success_rate * 100}
                suffix="%"
                prefix={
                  ep.success_rate > 0.9 ? (
                    <CheckCircleOutlined style={{ color: "#52c41a" }} />
                  ) : ep.success_rate > 0.5 ? (
                    <ThunderboltOutlined style={{ color: "#faad14" }} />
                  ) : (
                    <CloseCircleOutlined style={{ color: "#ff4d4f" }} />
                  )
                }
              />
              <div style={{ marginTop: 8, fontSize: 12, color: "#888" }}>
                <div>成功: {ep.success_count} | 失败: {ep.fail_count}</div>
                <div>延迟 EMA: {ep.latency_ema_ms.toFixed(1)}ms</div>
                <div>连续错误: {ep.consecutive_errors}</div>
              </div>
            </Card>
          </Col>
        ))}

        {(!health?.endpoints || health.endpoints.length === 0) && (
          <Col span={24}>
            <Text type="secondary">暂无端点数据。代理服务启动后将自动记录端点健康状态。</Text>
          </Col>
        )}
      </Row>
    </Card>
  );
}

// ---- Settings Panel ----

function SettingsPanel() {
  const kiro = useKiroAdmin();
  const [thinking, setThinkingState] = useState<KiroThinkingConfig | null>(null);
  const [settings, setSettingsState] = useState<KiroSettings | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [t, s] = await Promise.all([kiro.getThinking(), kiro.getSettings()]);
      setThinkingState(t);
      setSettingsState(s);
    } catch {
      // handled
    }
  }, [kiro]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const handleThinkingChange = async (mode: string) => {
    try {
      await kiro.setThinking(mode);
      message.success("Thinking 模式已更新");
      setThinkingState({ mode });
    } catch {
      // handled
    }
  };

  const handleSettingsChange = async (field: string, value: unknown) => {
    try {
      await kiro.setSettings(
        field === "preferred_endpoint" ? (value as string) : settings?.preferred_endpoint,
        field === "endpoint_fallback" ? (value as boolean) : settings?.endpoint_fallback
      );
      message.success("设置已更新");
      refresh();
    } catch {
      // handled
    }
  };

  return (
    <Space direction="vertical" style={{ width: "100%" }} size="middle">
      {kiro.error && (
        <Alert type="error" message={kiro.error} closable onClose={() => kiro.setError(null)} />
      )}

      <Card title="Thinking 模式" size="small">
        <Form layout="inline">
          <Form.Item label="模式">
            <Select
              value={thinking?.mode ?? "as_reasoning_content"}
              onChange={handleThinkingChange}
              style={{ width: 220 }}
              options={[
                { label: "Reasoning Content (默认)", value: "as_reasoning_content" },
                { label: "移除 Thinking", value: "remove" },
                { label: "保留标签", value: "pass" },
                { label: "去除标签保留内容", value: "strip_tags" },
              ]}
            />
          </Form.Item>
        </Form>
      </Card>

      <Card title="端点配置" size="small">
        <Form layout="inline">
          <Form.Item label="首选端点">
            <Select
              value={settings?.preferred_endpoint ?? "auto"}
              onChange={(v) => handleSettingsChange("preferred_endpoint", v)}
              style={{ width: 180 }}
              options={[
                { label: "Auto (自动降级)", value: "auto" },
                { label: "Kiro IDE", value: "kiro" },
                { label: "CodeWhisperer", value: "codewhisperer" },
                { label: "AmazonQ", value: "amazonq" },
              ]}
            />
          </Form.Item>
          <Form.Item label="429 降级">
            <Switch
              checked={settings?.endpoint_fallback !== false}
              onChange={(v) => handleSettingsChange("endpoint_fallback", v)}
            />
          </Form.Item>
        </Form>
      </Card>

      <Card title="负载均衡" size="small">
        <LoadBalanceConfig kiro={kiro} />
      </Card>
    </Space>
  );
}

function LoadBalanceConfig({ kiro }: { kiro: ReturnType<typeof useKiroAdmin> }) {
  const [mode, setMode] = useState<string>("priority");

  useEffect(() => {
    kiro.getLbConfig().then((data: unknown) => {
      const d = data as Record<string, unknown>;
      if (d?.mode) setMode(d.mode as string);
    }).catch(() => {});
  }, [kiro]);

  const handleChange = async (newMode: string) => {
    try {
      await kiro.setLbConfig(newMode);
      setMode(newMode);
      message.success("负载均衡模式已更新");
    } catch {
      // handled
    }
  };

  return (
    <Form layout="inline">
      <Form.Item label="模式">
        <Select
          value={mode}
          onChange={handleChange}
          style={{ width: 180 }}
          options={[
            { label: "Priority (按优先级)", value: "priority" },
            { label: "Balanced (轮询)", value: "balanced" },
            { label: "Smart (智能评分)", value: "smart" },
          ]}
        />
      </Form.Item>
    </Form>
  );
}

// ---- Auth Flows ----

function AuthFlows() {
  const kiro = useKiroAdmin();
  const [ssoTokens, setSsoTokens] = useState("");
  const [ssoRegion, setSsoRegion] = useState("us-east-1");
  const [iamStartUrl, setIamStartUrl] = useState("");
  const [iamRegion, setIamRegion] = useState("us-east-1");
  const [iamSession, setIamSession] = useState<string | null>(null);
  const [iamCallbackUrl, setIamCallbackUrl] = useState("");

  const handleSsoImport = async () => {
    if (!ssoTokens.trim()) {
      message.warning("请输入 SSO token");
      return;
    }
    try {
      const result = await kiro.importSsoTokens(ssoTokens, ssoRegion) as Record<string, unknown>;
      const count = (result?.imported as unknown[])?.length ?? 0;
      message.success(`成功导入 ${count} 个账户`);
      setSsoTokens("");
    } catch {
      // handled
    }
  };

  const handleIamStart = async () => {
    if (!iamStartUrl.trim()) {
      message.warning("请输入 IAM Identity Center Start URL");
      return;
    }
    try {
      const result = await kiro.startIamSso(iamStartUrl, iamRegion) as Record<string, unknown>;
      setIamSession(result?.session_id as string);
      const url = result?.authorize_url as string;
      if (url) {
        Modal.info({
          title: "请在浏览器中完成登录",
          content: (
            <div>
              <p>请在浏览器中打开以下链接完成登录：</p>
              <Input.TextArea value={url} readOnly rows={3} />
              <p style={{ marginTop: 8 }}>登录完成后，将回调 URL 粘贴到下方。</p>
            </div>
          ),
          width: 600,
        });
      }
    } catch {
      // handled
    }
  };

  const handleIamComplete = async () => {
    if (!iamSession || !iamCallbackUrl.trim()) {
      message.warning("请先启动登录流程，并输入回调 URL");
      return;
    }
    try {
      await kiro.completeIamSso(iamSession, iamCallbackUrl);
      message.success("IAM IdC 登录成功，账户已添加");
      setIamSession(null);
      setIamCallbackUrl("");
    } catch {
      // handled
    }
  };

  return (
    <Space direction="vertical" style={{ width: "100%" }} size="middle">
      {kiro.error && (
        <Alert type="error" message={kiro.error} closable onClose={() => kiro.setError(null)} />
      )}

      <Card title="SSO Token 导入" size="small" extra={<SafetyOutlined />}>
        <Text type="secondary" style={{ display: "block", marginBottom: 12 }}>
          粘贴 SSO Bearer Token（多个用换行分隔），系统将自动完成设备授权流程。
        </Text>
        <Form layout="vertical">
          <Form.Item label="SSO Token(s)">
            <Input.TextArea
              value={ssoTokens}
              onChange={(e) => setSsoTokens(e.target.value)}
              rows={4}
              placeholder={"token1\ntoken2\ntoken3"}
            />
          </Form.Item>
          <Form.Item label="区域">
            <Select
              value={ssoRegion}
              onChange={setSsoRegion}
              style={{ width: 200 }}
              options={[
                { label: "us-east-1", value: "us-east-1" },
                { label: "us-west-2", value: "us-west-2" },
                { label: "eu-west-1", value: "eu-west-1" },
              ]}
            />
          </Form.Item>
          <Form.Item>
            <Button
              type="primary"
              icon={<ApiOutlined />}
              onClick={handleSsoImport}
              loading={kiro.loading}
            >
              导入
            </Button>
          </Form.Item>
        </Form>
      </Card>

      <Card title="IAM Identity Center 登录" size="small" extra={<SafetyOutlined />}>
        <Text type="secondary" style={{ display: "block", marginBottom: 12 }}>
          适用于企业 AWS SSO (IdC) 用户，通过 PKCE 授权码流程登录。
        </Text>
        <Form layout="vertical">
          <Form.Item label="Start URL">
            <Input
              value={iamStartUrl}
              onChange={(e) => setIamStartUrl(e.target.value)}
              placeholder="https://your-sso-portal.awsapps.com/start"
            />
          </Form.Item>
          <Form.Item label="区域">
            <Select
              value={iamRegion}
              onChange={setIamRegion}
              style={{ width: 200 }}
              options={[
                { label: "us-east-1", value: "us-east-1" },
                { label: "us-west-2", value: "us-west-2" },
                { label: "eu-west-1", value: "eu-west-1" },
              ]}
            />
          </Form.Item>
          <Form.Item>
            <Button
              type="primary"
              icon={<ApiOutlined />}
              onClick={handleIamStart}
              loading={kiro.loading}
              disabled={!!iamSession}
            >
              {iamSession ? "已启动" : "启动登录"}
            </Button>
          </Form.Item>

          {iamSession && (
            <>
              <Form.Item label="回调 URL">
                <Input
                  value={iamCallbackUrl}
                  onChange={(e) => setIamCallbackUrl(e.target.value)}
                  placeholder="http://127.0.0.1/oauth/callback?code=...&state=..."
                />
              </Form.Item>
              <Form.Item>
                <Button onClick={handleIamComplete} loading={kiro.loading}>
                  完成登录
                </Button>
              </Form.Item>
            </>
          )}
        </Form>
      </Card>
    </Space>
  );
}
