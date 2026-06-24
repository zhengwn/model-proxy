import { useState, useEffect, useCallback, useRef } from "react";
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
  Statistic,
  Row,
  Col,
  Progress,
  message,
  Popconfirm,
  Tooltip,
  Alert,
  Typography,
  Tabs,
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
  TeamOutlined,
  SettingOutlined,
  InfoCircleOutlined,
} from "@ant-design/icons";
import { useKiroAdmin } from "../hooks/useKiroAdmin";
import { useLocale } from "../i18n";
import type { KiroCredential, KiroEndpointHealth, KiroThinkingConfig, KiroSettings } from "../types";

const { Text } = Typography;

export default function KiroPanel() {
  const { t } = useLocale();

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
      {/* Main Tabbed Panels */}
      <Tabs
        defaultActiveKey="accounts"
        type="card"
        items={[
          {
            key: "accounts",
            label: (
              <span>
                <TeamOutlined style={{ marginRight: 6 }} />
                {t("kiro.tabAccounts")}
              </span>
            ),
            children: (
              <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
                <CredentialManager />
                <AuthFlows />
              </div>
            ),
          },
          {
            key: "endpoints",
            label: (
              <span>
                <ThunderboltOutlined style={{ marginRight: 6 }} />
                {t("kiro.tabEndpoints")}
              </span>
            ),
            children: (
              <EndpointDashboard />
            ),
          },
          {
            key: "settings",
            label: (
              <span>
                <SettingOutlined style={{ marginRight: 6 }} />
                {t("kiro.tabSettings")}
              </span>
            ),
            children: (
              <SettingsPanel />
            ),
          },
        ]}
      />
    </div>
  );
}

// ---- Credential Manager ----

function CredentialManager() {
  const kiro = useKiroAdmin();
  const { t } = useLocale();
  const [creds, setCreds] = useState<KiroCredential[]>([]);
  const [credLoading, setCredLoading] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [detailId, setDetailId] = useState<string | null>(null);
  const [detail, setDetail] = useState<unknown>(null);
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const isMounted = useRef(true);

  useEffect(() => {
    isMounted.current = true;
    return () => { isMounted.current = false; };
  }, []);

  const { listCredentials, addCredential, testCredential, deleteCredential, setDisabled, batchCredentials, refreshCredential, getCredentialFull } = kiro;

  const refresh = useCallback(async () => {
    if (isMounted.current) setCredLoading(true);
    try {
      const data = await listCredentials();
      const list = Array.isArray(data) ? data : (data as Record<string, unknown>)?.credentials ?? [];
      if (isMounted.current) setCreds(list as KiroCredential[]);
    } catch {
      // error shown by hook
    } finally {
      if (isMounted.current) setCredLoading(false);
    }
  }, [listCredentials]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const handleTest = async (id: string) => {
    try {
      const result = (await testCredential(id)) as unknown as Record<string, unknown>;
      if (result?.success) {
        message.success(t("kiro.testPassed", { latency: result.latency_ms as number }));
      } else {
        message.error(t("kiro.testFailed", { error: (result?.error as string) ?? t("error.unknown") }));
      }
    } catch {
      // handled
    }
  };

  const handleDelete = async (id: string) => {
    try {
      await deleteCredential(id);
      message.success(t("kiro.deleted"));
      refresh();
    } catch {
      // handled
    }
  };

  const handleToggle = async (id: string, disabled: boolean) => {
    try {
      await setDisabled(id, disabled);
      message.success(disabled ? t("kiro.disabled") : t("kiro.enabled"));
      refresh();
    } catch {
      // handled
    }
  };

  const handleBatch = async (action: string) => {
    if (selectedIds.length === 0) {
      message.warning(t("kiro.selectAccountsFirst"));
      return;
    }
    try {
      const result = (await batchCredentials(selectedIds, action)) as unknown as Record<string, unknown>;
      message.success(t("kiro.batchDone", { count: ((result?.results as string[])?.length ?? 0) as number }));
      setSelectedIds([]);
      refresh();
    } catch {
      // handled
    }
  };

  const handleDetail = async (id: string) => {
    try {
      const data = await getCredentialFull(id);
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
      ellipsis: true,
      render: (id: string) => (
        <Button type="link" size="small" onClick={() => handleDetail(id)}>
          {id}
        </Button>
      ),
    },
    {
      title: t("kiro.status"),
      key: "status",
      width: 140,
      render: (_: unknown, record: KiroCredential) => (
        <Space>
          {record.disabled ? (
            <Tag color="red">{t("kiro.disabled")}</Tag>
          ) : record.is_available ? (
            <Tag color="green">{t("kiro.available")}</Tag>
          ) : (
            <Tag color="orange">{t("kiro.unavailable")}</Tag>
          )}
          {record.is_current && <Tag color="blue">{t("kiro.current")}</Tag>}
        </Space>
      ),
    },
    {
      title: t("kiro.region"),
      dataIndex: "region",
      key: "region",
      width: 100,
    },
    {
      title: t("kiro.priority"),
      dataIndex: "priority",
      key: "priority",
      width: 100,
      sorter: (a: KiroCredential, b: KiroCredential) => a.priority - b.priority,
    },
    {
      title: t("kiro.healthScore"),
      dataIndex: "health_score",
      key: "health_score",
      width: 120,
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
      title: t("kiro.requests"),
      key: "requests",
      width: 100,
      render: (_: unknown, r: KiroCredential) => (
        <Tooltip title={t("kiro.successFail", { s: r.successful_requests, f: r.failed_requests })}>
          <Text>{r.total_requests}</Text>
        </Tooltip>
      ),
    },
    {
      title: t("kiro.actions"),
      key: "actions",
      width: 260,
      render: (_: unknown, record: KiroCredential) => (
        <Space size="small">
          <Button size="small" icon={<ExperimentOutlined />} onClick={() => handleTest(record.id)}>
            {t("kiro.test")}
          </Button>
          <Button
            size="small"
            icon={<ReloadOutlined />}
            onClick={async () => {
              try {
                await refreshCredential(record.id);
                message.success(t("kiro.refreshed"));
                refresh();
              } catch {}
            }}
          >
            {t("common.refresh")}
          </Button>
          <Button
            size="small"
            onClick={() => handleToggle(record.id, !record.disabled)}
          >
            {record.disabled ? t("common.enable") : t("common.disable")}
          </Button>
          <Popconfirm title={t("kiro.confirmDelete")} onConfirm={() => handleDelete(record.id)}>
            <Button size="small" danger icon={<DeleteOutlined />} />
          </Popconfirm>
        </Space>
      ),
    },
  ];

  return (
    <Card
      title={t("kiro.accountManager")}
      extra={
        <Space>
          {selectedIds.length > 0 && (
            <>
              <Button onClick={() => handleBatch("enable")}>{t("kiro.batchEnable")}</Button>
              <Button onClick={() => handleBatch("disable")}>{t("kiro.batchDisable")}</Button>
              <Button onClick={() => handleBatch("refresh")}>{t("kiro.batchRefresh")}</Button>
            </>
          )}
          <Button icon={<ReloadOutlined />} onClick={refresh}>
            {t("common.refresh")}
          </Button>
          <Button type="primary" icon={<PlusOutlined />} onClick={() => setAddOpen(true)}>
            {t("kiro.addAccount")}
          </Button>
        </Space>
      }
    >
      <div style={{ marginBottom: 16, display: "flex", alignItems: "center", gap: 8 }}>
        <InfoCircleOutlined style={{ color: "#1677ff" }} />
        <Text type="secondary" style={{ fontSize: 13 }}>
          {t("kiro.accountManagerDesc")}
        </Text>
      </div>

      {kiro.error && (
        <Alert
          type="error"
          message={kiro.error}
          closable
          onClose={() => kiro.setError(null)}
          style={{ marginBottom: 16 }}
          description={t("kiro.startServiceHint")}
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
          onChange: (keys) => setSelectedIds(keys as string[]),
        }}
        loading={credLoading}
        scroll={{ x: "max-content" }}
        locale={{
          emptyText: <div style={{ padding: '40px 0', color: '#888' }}>{t("kiro.emptyCredentials")}</div>
        }}
      />

      <AddCredentialModal
        open={addOpen}
        onClose={() => setAddOpen(false)}
        onAdded={() => {
          setAddOpen(false);
          refresh();
        }}
        addCredential={addCredential}
        loading={kiro.loading}
      />

      <Modal
        title={t("kiro.accountDetail", { id: detailId ?? "" })}
        open={!!detailId}
        onCancel={() => {
          setDetailId(null);
          setDetail(null);
        }}
        footer={null}
        width={600}
      >
        <pre style={{ maxHeight: 400, overflow: "auto", fontSize: 12 }}>
          {detail ? JSON.stringify(detail, null, 2) : t("common.loading")}
        </pre>
      </Modal>
    </Card>
  );
}

function AddCredentialModal({
  open,
  onClose,
  onAdded,
  addCredential,
  loading,
}: {
  open: boolean;
  onClose: () => void;
  onAdded: () => void;
  addCredential: ReturnType<typeof useKiroAdmin>['addCredential'];
  loading: boolean;
}) {
  const [form] = Form.useForm();
  const { t } = useLocale();

  const handleOk = async () => {
    try {
      const values = await form.validateFields();
      await addCredential(
        values.refresh_token,
        values.auth_method,
        values.region,
        values.priority
      );
      message.success(t("kiro.accountAdded"));
      form.resetFields();
      onAdded();
    } catch {
      // validation or API error
    }
  };

  return (
    <Modal
      title={t("kiro.addAccountTitle")}
      open={open}
      onCancel={onClose}
      onOk={handleOk}
      confirmLoading={loading}
    >
      <Form form={form} layout="vertical">
        <Form.Item
          name="refresh_token"
          label="Refresh Token"
          rules={[{ required: true, message: t("kiro.refreshTokenRequired") }]}
        >
          <Input.TextArea rows={3} placeholder={t("kiro.refreshTokenPlaceholder")} />
        </Form.Item>
        <Form.Item name="auth_method" label={t("kiro.authMethod")} initialValue="social">
          <Select
            options={[
              { label: "Social (Kiro Desktop)", value: "social" },
              { label: "IdC (AWS SSO)", value: "idc" },
              { label: "API Key", value: "api_key" },
            ]}
          />
        </Form.Item>
        <Form.Item name="region" label={t("kiro.regionLabel")} initialValue="us-east-1">
          <Select
            options={[
              { label: "us-east-1", value: "us-east-1" },
              { label: "us-west-2", value: "us-west-2" },
              { label: "eu-west-1", value: "eu-west-1" },
              { label: "ap-northeast-1", value: "ap-northeast-1" },
            ]}
          />
        </Form.Item>
        <Form.Item name="priority" label={t("kiro.priorityLabel")} initialValue={0}>
          <Input type="number" placeholder={t("kiro.priorityPlaceholder")} />
        </Form.Item>
      </Form>
    </Modal>
  );
}

// ---- Endpoint Dashboard ----

function EndpointDashboard() {
  const kiro = useKiroAdmin();
  const { t } = useLocale();
  const [health, setHealth] = useState<KiroEndpointHealth | null>(null);
  const { getEndpointHealth } = kiro;

  const refresh = useCallback(async () => {
    try {
      const data = await getEndpointHealth();
      setHealth(data);
    } catch {
      // handled
    }
  }, [getEndpointHealth]);

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 10000);
    return () => clearInterval(interval);
  }, [refresh]);

  const endpoints = health?.endpoints ?? [];
  const hasData = endpoints.length > 0;

  // Calculate aggregated stats
  let totalSuccess = 0;
  let totalFail = 0;
  let maxConsecutiveErrors = 0;
  let totalLatency = 0;
  let latencyCount = 0;

  endpoints.forEach((ep) => {
    totalSuccess += ep.success_count;
    totalFail += ep.fail_count;
    if (ep.consecutive_errors > maxConsecutiveErrors) {
      maxConsecutiveErrors = ep.consecutive_errors;
    }
    if (ep.latency_ema_ms > 0.0) {
      totalLatency += ep.latency_ema_ms;
      latencyCount += 1;
    }
  });

  const totalRequests = totalSuccess + totalFail;
  const overallSuccessRate = totalRequests > 0 ? totalSuccess / totalRequests : 1.0;
  const avgLatency = latencyCount > 0 ? totalLatency / latencyCount : 0.0;

  return (
    <Card
      title={t("kiro.endpointHealth")}
      extra={
        <Button icon={<ReloadOutlined />} onClick={refresh}>
          {t("common.refresh")}
        </Button>
      }
    >
      <div style={{ marginBottom: 16, display: "flex", alignItems: "center", gap: 8 }}>
        <InfoCircleOutlined style={{ color: "#1677ff" }} />
        <Text type="secondary" style={{ fontSize: 13 }}>
          {t("kiro.endpointHealthDesc")}
        </Text>
      </div>

      {kiro.error && (
        <Alert type="error" message={kiro.error} closable onClose={() => kiro.setError(null)} style={{ marginBottom: 16 }} />
      )}

      <Row gutter={[16, 16]}>
        {hasData ? (
          <Col span={24}>
            <Card size="small" style={{ background: "rgba(22, 119, 255, 0.02)" }}>
              <Row align="middle" gutter={[24, 16]}>
                <Col xs={24} sm={8}>
                  <Statistic
                    title="Kiro API 服务连接状态 (runtime.kiro.dev)"
                    value={overallSuccessRate * 100}
                    precision={1}
                    suffix="%"
                    prefix={
                      overallSuccessRate > 0.9 ? (
                        <CheckCircleOutlined style={{ color: "#52c41a" }} />
                      ) : overallSuccessRate > 0.5 ? (
                        <ThunderboltOutlined style={{ color: "#faad14" }} />
                      ) : (
                        <CloseCircleOutlined style={{ color: "#ff4d4f" }} />
                      )
                    }
                  />
                </Col>
                <Col xs={12} sm={8}>
                  <Statistic
                    title="平均响应延迟"
                    value={avgLatency}
                    precision={1}
                    suffix=" ms"
                  />
                </Col>
                <Col xs={12} sm={8}>
                  <div style={{ fontSize: 13, color: "#888", display: "flex", flexDirection: "column", justifyContent: "center" }}>
                    <div>总请求数: <strong style={{ color: "#333" }}>{totalRequests}</strong> (成功: {totalSuccess} | 失败: {totalFail})</div>
                    <div>最大连续错误数: <strong style={{ color: "#333" }}>{maxConsecutiveErrors}</strong></div>
                  </div>
                </Col>
              </Row>
            </Card>
          </Col>
        ) : (
          <Col span={24}>
            <Text type="secondary">{t("kiro.noEndpointData")}</Text>
          </Col>
        )}
      </Row>
    </Card>
  );
}

// ---- Settings Panel ----

function SettingsPanel() {
  const kiro = useKiroAdmin();
  const { t } = useLocale();
  const [thinking, setThinkingState] = useState<KiroThinkingConfig | null>(null);
  const [settings, setSettingsState] = useState<KiroSettings | null>(null);
  const { getThinking, getSettings, setThinking, setSettings } = kiro;

  const refresh = useCallback(async () => {
    try {
      const [thinkingData, s] = await Promise.all([getThinking(), getSettings()]);
      setThinkingState(thinkingData);
      setSettingsState(s);
    } catch {
      // handled
    }
  }, [getThinking, getSettings]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const handleThinkingChange = async (mode: string) => {
    try {
      await setThinking(mode);
      message.success(t("kiro.thinkingUpdated"));
      setThinkingState({ mode });
    } catch {
      // handled
    }
  };

  const handleSettingsChange = async (field: string, value: unknown) => {
    try {
      await setSettings(
        field === "preferred_endpoint" ? (value as string) : settings?.preferred_endpoint,
        field === "endpoint_fallback" ? (value as boolean) : settings?.endpoint_fallback
      );
      message.success(t("kiro.settingsUpdated"));
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

      <Card title={t("kiro.thinkingMode")}>
        <div style={{ marginBottom: 16, display: "flex", alignItems: "start", gap: 8 }}>
          <InfoCircleOutlined style={{ color: "#1677ff", marginTop: 3 }} />
          <Text type="secondary" style={{ fontSize: 13 }}>
            {t("kiro.thinkingModeDesc")}
          </Text>
        </div>
        <Form layout="inline">
          <Form.Item label={t("kiro.mode")}>
            <Select
              value={thinking?.mode ?? "as_reasoning_content"}
              onChange={handleThinkingChange}
              style={{ width: 220 }}
              options={[
                { label: t("kiro.modeReasoningContent"), value: "as_reasoning_content" },
                { label: t("kiro.modeRemove"), value: "remove" },
                { label: t("kiro.modePass"), value: "pass" },
                { label: t("kiro.modeStripTags"), value: "strip_tags" },
              ]}
            />
          </Form.Item>
        </Form>
      </Card>

      <Card title={t("kiro.endpointConfig")}>
        <div style={{ marginBottom: 16, display: "flex", alignItems: "start", gap: 8 }}>
          <InfoCircleOutlined style={{ color: "#1677ff", marginTop: 3 }} />
          <Text type="secondary" style={{ fontSize: 13 }}>
            {t("kiro.endpointConfigDesc")}
          </Text>
        </div>
        <Form layout="inline">
          <Form.Item label={t("kiro.preferredEndpoint")}>
            <Select
              value={settings?.preferred_endpoint ?? "auto"}
              onChange={(v) => handleSettingsChange("preferred_endpoint", v)}
              style={{ width: 180 }}
              options={[
                { label: t("kiro.autoDowngrade"), value: "auto" },
                { label: "Kiro IDE", value: "kiro" },
                { label: "CodeWhisperer", value: "codewhisperer" },
                { label: "AmazonQ", value: "amazonq" },
              ]}
            />
          </Form.Item>
          <Form.Item label={t("kiro.fallback429")}>
            <Switch
              checked={settings?.endpoint_fallback !== false}
              onChange={(v) => handleSettingsChange("endpoint_fallback", v)}
            />
          </Form.Item>
        </Form>
      </Card>

      <Card title={t("kiro.loadBalance")}>
        <div style={{ marginBottom: 16, display: "flex", alignItems: "start", gap: 8 }}>
          <InfoCircleOutlined style={{ color: "#1677ff", marginTop: 3 }} />
          <Text type="secondary" style={{ fontSize: 13 }}>
            {t("kiro.loadBalanceDesc")}
          </Text>
        </div>
        <LoadBalanceConfig getLbConfig={kiro.getLbConfig} setLbConfig={kiro.setLbConfig} />
      </Card>
    </Space>
  );
}

function LoadBalanceConfig({ getLbConfig, setLbConfig }: { 
  getLbConfig: ReturnType<typeof useKiroAdmin>['getLbConfig'],
  setLbConfig: ReturnType<typeof useKiroAdmin>['setLbConfig'] 
}) {
  const { t } = useLocale();
  const [mode, setMode] = useState<string>("priority");

  useEffect(() => {
    getLbConfig().then((data: unknown) => {
      const d = data as Record<string, unknown>;
      if (d?.mode) setMode(d.mode as string);
    }).catch(() => {});
  }, [getLbConfig]);

  const handleChange = async (newMode: string) => {
    try {
      await setLbConfig(newMode);
      setMode(newMode);
      message.success(t("kiro.lbUpdated"));
    } catch {
      // handled
    }
  };

  return (
    <Form layout="inline">
      <Form.Item label={t("kiro.mode")}>
        <Select
          value={mode}
          onChange={handleChange}
          style={{ width: 180 }}
          options={[
            { label: t("kiro.lbPriority"), value: "priority" },
            { label: t("kiro.lbBalanced"), value: "balanced" },
            { label: t("kiro.lbSmart"), value: "smart" },
          ]}
        />
      </Form.Item>
    </Form>
  );
}

// ---- Auth Flows ----

function AuthFlows() {
  const kiro = useKiroAdmin();
  const { t } = useLocale();
  const [ssoTokens, setSsoTokens] = useState("");
  const [ssoRegion, setSsoRegion] = useState("us-east-1");
  const [iamStartUrl, setIamStartUrl] = useState("");
  const [iamRegion, setIamRegion] = useState("us-east-1");
  const [iamSession, setIamSession] = useState<string | null>(null);
  const [iamCallbackUrl, setIamCallbackUrl] = useState("");

  const handleSsoImport = async () => {
    if (!ssoTokens.trim()) {
      message.warning(t("kiro.ssoTokenRequired"));
      return;
    }
    try {
      const result = await kiro.importSsoTokens(ssoTokens, ssoRegion) as Record<string, unknown>;
      const count = (result?.imported as unknown[])?.length ?? 0;
      message.success(t("kiro.ssoImported", { count }));
      setSsoTokens("");
    } catch {
      // handled
    }
  };

  const handleIamStart = async () => {
    if (!iamStartUrl.trim()) {
      message.warning(t("kiro.iamStartUrlRequired"));
      return;
    }
    try {
      const result = await kiro.startIamSso(iamStartUrl, iamRegion) as Record<string, unknown>;
      setIamSession(result?.session_id as string);
      const url = result?.authorize_url as string;
      if (url) {
        Modal.info({
          title: t("kiro.iamLoginBrowser"),
          content: (
            <div>
              <p>{t("kiro.iamLoginOpenLink")}</p>
              <Input.TextArea value={url} readOnly rows={3} />
              <p style={{ marginTop: 8 }}>{t("kiro.iamLoginPasteCallback")}</p>
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
      message.warning(t("kiro.iamStartFirst"));
      return;
    }
    try {
      await kiro.completeIamSso(iamSession, iamCallbackUrl);
      message.success(t("kiro.iamLoginSuccess"));
      setIamSession(null);
      setIamCallbackUrl("");
    } catch {
      // handled
    }
  };

  return (
    <Row gutter={[16, 16]}>
      {kiro.error && (
        <Col span={24}>
          <Alert type="error" message={kiro.error} closable onClose={() => kiro.setError(null)} />
        </Col>
      )}

      <Col xs={24} md={12}>
        <Card title={t("kiro.ssoImport")} extra={<SafetyOutlined />} style={{ height: "100%" }}>
          <Text type="secondary" style={{ display: "block", marginBottom: 12, fontSize: 12 }}>
            {t("kiro.ssoImportDesc")}
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
            <Form.Item label={t("kiro.regionLabel")}>
              <Select
                value={ssoRegion}
                onChange={setSsoRegion}
                style={{ width: "100%" }}
                options={[
                  { label: "us-east-1", value: "us-east-1" },
                  { label: "us-west-2", value: "us-west-2" },
                  { label: "eu-west-1", value: "eu-west-1" },
                ]}
              />
            </Form.Item>
            <Form.Item style={{ marginBottom: 0 }}>
              <Button
                type="primary"
                icon={<ApiOutlined />}
                onClick={handleSsoImport}
                loading={kiro.loading}
              >
                {t("kiro.import")}
              </Button>
            </Form.Item>
          </Form>
        </Card>
      </Col>

      <Col xs={24} md={12}>
        <Card title={t("kiro.iamLogin")} extra={<SafetyOutlined />} style={{ height: "100%" }}>
          <Text type="secondary" style={{ display: "block", marginBottom: 12, fontSize: 12 }}>
            {t("kiro.iamLoginDesc")}
          </Text>
          <Form layout="vertical">
            <Form.Item label="Start URL">
              <Input
                value={iamStartUrl}
                onChange={(e) => setIamStartUrl(e.target.value)}
                placeholder="https://your-sso-portal.awsapps.com/start"
              />
            </Form.Item>
            <Form.Item label={t("kiro.regionLabel")}>
              <Select
                value={iamRegion}
                onChange={setIamRegion}
                style={{ width: "100%" }}
                options={[
                  { label: "us-east-1", value: "us-east-1" },
                  { label: "us-west-2", value: "us-west-2" },
                  { label: "eu-west-1", value: "eu-west-1" },
                ]}
              />
            </Form.Item>
            <Form.Item style={{ marginBottom: iamSession ? 16 : 0 }}>
              <Button
                type="primary"
                icon={<ApiOutlined />}
                onClick={handleIamStart}
                loading={kiro.loading}
                disabled={!!iamSession}
              >
                {iamSession ? t("kiro.started") : t("kiro.startLogin")}
              </Button>
            </Form.Item>

            {iamSession && (
              <>
                <Form.Item label={t("kiro.callbackUrl")}>
                  <Input
                    value={iamCallbackUrl}
                    onChange={(e) => setIamCallbackUrl(e.target.value)}
                    placeholder="http://127.0.0.1/oauth/callback?code=...&state=..."
                  />
                </Form.Item>
                <Form.Item style={{ marginBottom: 0 }}>
                  <Button onClick={handleIamComplete} loading={kiro.loading}>
                    {t("kiro.completeLogin")}
                  </Button>
                </Form.Item>
              </>
            )}
          </Form>
        </Card>
      </Col>
    </Row>
  );
}
