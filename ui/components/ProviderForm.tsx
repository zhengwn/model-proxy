import { useEffect, useMemo, useState } from "react";
import {
  Form,
  Input,
  Select,
  Switch,
  Button,
  Card,
  Space,
  Collapse,
  Typography,
  Alert,
  message,
  InputNumber,
} from "antd";
import { ThunderboltOutlined } from "@ant-design/icons";
import { invoke } from "@tauri-apps/api/core";
import type { KiroConfig, ProviderConfig, ProviderFormat, TestProviderResult } from "../types";

const { Text } = Typography;

interface ProviderFormProps {
  mode: "add" | "edit";
  initialValues?: ProviderConfig;
  existingNames: string[];
  onSubmit: (config: ProviderConfig) => Promise<void>;
  onCancel: () => void;
  /** Auto-select this template on mount (add mode only) */
  defaultTemplate?: string;
}

const defaultQuirks = {
  reasoning_all_or_nothing: false,
  no_json_schema: false,
  supports_reasoning_effort: false,
  max_reasoning_effort: "high",
};

const defaultKiroConfig: KiroConfig = {
  auth_method: "social",
  region: "us-east-1",
  thinking_mode: "as_reasoning_content",
  preferred_endpoint: "auto",
  endpoint_fallback: true,
};

interface ProviderTemplate {
  label: string;
  name: string;
  base_url: string;
  model: string;
  format: ProviderFormat;
  quirks: typeof defaultQuirks;
  kiro_config?: KiroConfig;
}

const PROVIDER_TEMPLATES: ProviderTemplate[] = [
  {
    label: "OpenAI",
    name: "openai",
    base_url: "https://api.openai.com/v1",
    model: "gpt-4o",
    format: "openai",
    quirks: defaultQuirks,
  },
  {
    label: "Anthropic",
    name: "anthropic",
    base_url: "https://api.anthropic.com",
    model: "claude-sonnet-4-20250514",
    format: "anthropic",
    quirks: defaultQuirks,
  },
  {
    label: "DeepSeek",
    name: "deepseek",
    base_url: "https://api.deepseek.com/v1",
    model: "deepseek-v4-pro",
    format: "openai",
    quirks: {
      reasoning_all_or_nothing: true,
      no_json_schema: true,
      supports_reasoning_effort: true,
      max_reasoning_effort: "max",
    },
  },
  {
    label: "Google Gemini",
    name: "gemini",
    base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
    model: "gemini-2.5-flash",
    format: "openai",
    quirks: defaultQuirks,
  },
  {
    label: "Azure OpenAI",
    name: "azure",
    base_url: "https://YOUR-RESOURCE.openai.azure.com/openai/deployments/YOUR-DEPLOYMENT",
    model: "gpt-4o",
    format: "openai",
    quirks: defaultQuirks,
  },
  {
    label: "Kiro / Amazon Q Developer",
    name: "kiro",
    base_url: "https://q.us-east-1.amazonaws.com",
    model: "claude-sonnet-4.5",
    format: "kiro",
    quirks: defaultQuirks,
    kiro_config: defaultKiroConfig,
  },
  {
    label: "自定义",
    name: "",
    base_url: "",
    model: "",
    format: "openai",
    quirks: defaultQuirks,
  },
];

export function ProviderForm({
  mode,
  initialValues,
  existingNames,
  onSubmit,
  onCancel,
  defaultTemplate,
}: ProviderFormProps) {
  const [form] = Form.useForm<ProviderConfig>();

  // Compute the initial form values with useMemo to avoid recalculation on every render
  const computedInitialValues = useMemo(() => {
    if (initialValues) return initialValues;
    if (defaultTemplate) {
      const template = PROVIDER_TEMPLATES.find((t) => t.label === defaultTemplate);
      if (template) {
        return {
          name: existingNames.includes(template.name) ? "" : template.name,
          base_url: template.base_url,
          api_key: "",
          model: template.model,
          format: template.format,
          quirks: template.quirks,
          kiro_config: template.kiro_config,
        } as ProviderConfig;
      }
    }
    return { quirks: defaultQuirks } as unknown as ProviderConfig;
  }, [initialValues, defaultTemplate, existingNames]);

  useEffect(() => {
    // Set basic fields
    form.setFieldsValue(computedInitialValues);
    // Explicitly set quirks fields to ensure Switch components render correctly
    const quirks = computedInitialValues.quirks || defaultQuirks;
    form.setFieldValue(["quirks", "reasoning_all_or_nothing"], quirks.reasoning_all_or_nothing);
    form.setFieldValue(["quirks", "no_json_schema"], quirks.no_json_schema);
    form.setFieldValue(["quirks", "supports_reasoning_effort"], quirks.supports_reasoning_effort);
    form.setFieldValue(["quirks", "max_reasoning_effort"], quirks.max_reasoning_effort);

    if (computedInitialValues.format === "kiro" || computedInitialValues.kiro_config) {
      const kiroConfig = {
        ...defaultKiroConfig,
        ...computedInitialValues.kiro_config,
      };
      form.setFieldValue("kiro_config", kiroConfig);
    }
  }, [form, computedInitialValues]);

  const providerFormat = Form.useWatch("format", form) ?? computedInitialValues.format;
  const kiroAuthMethod =
    Form.useWatch(["kiro_config", "auth_method"], form) ??
    computedInitialValues.kiro_config?.auth_method ??
    defaultKiroConfig.auth_method;
  const isKiro = providerFormat === "kiro";

  const handleFinish = async (values: ProviderConfig) => {
    const config: ProviderConfig = {
      ...values,
      api_key: values.api_key || "",
      quirks: {
        ...defaultQuirks,
        ...values.quirks,
      },
    };

    if (config.format === "kiro") {
      config.kiro_config = {
        ...defaultKiroConfig,
        ...values.kiro_config,
      };
    } else {
      delete config.kiro_config;
    }

    await onSubmit(config);
  };

  const handleTemplateSelect = (templateLabel: string) => {
    const template = PROVIDER_TEMPLATES.find((t) => t.label === templateLabel);
    if (template) {
      form.setFieldsValue({
        name: existingNames.includes(template.name) ? "" : template.name,
        base_url: template.base_url,
        api_key: template.format === "kiro" ? "" : form.getFieldValue("api_key"),
        model: template.model,
        format: template.format,
        kiro_config: template.kiro_config,
      });
      // Set quirks fields individually to ensure Switch components update correctly
      form.setFieldValue(["quirks", "reasoning_all_or_nothing"], template.quirks.reasoning_all_or_nothing);
      form.setFieldValue(["quirks", "no_json_schema"], template.quirks.no_json_schema);
      form.setFieldValue(["quirks", "supports_reasoning_effort"], template.quirks.supports_reasoning_effort);
      form.setFieldValue(["quirks", "max_reasoning_effort"], template.quirks.max_reasoning_effort);
      if (template.kiro_config) {
        form.setFieldValue("kiro_config", {
          ...defaultKiroConfig,
          ...template.kiro_config,
        });
      } else {
        form.setFieldValue("kiro_config", undefined);
      }
    }
  };

  return (
    <Form
      form={form}
      layout="vertical"
      onFinish={handleFinish}
      autoComplete="off"
      style={{ maxWidth: 640 }}
      initialValues={computedInitialValues}
    >
      <Card title="基本信息" size="small" style={{ marginBottom: 16 }}>
        {mode === "add" && (
          <Form.Item label="从模板创建">
            <Select
              placeholder="选择 Provider 模板快速填充"
              defaultValue={defaultTemplate}
              allowClear
              onChange={handleTemplateSelect}
              options={PROVIDER_TEMPLATES.map((t) => ({
                value: t.label,
                label: t.label,
              }))}
              style={{ width: 240 }}
            />
            <div style={{ marginTop: 4 }}>
              <Text type="secondary" style={{ fontSize: 12 }}>
                选择模板后只需填写 API Key 即可
              </Text>
            </div>
          </Form.Item>
        )}
        <Form.Item
          label="名称"
          name="name"
          rules={[
            { required: true, message: "请输入 Provider 名称" },
            { max: 64, message: "名称不能超过 64 个字符" },
            {
              validator: (_, value) => {
                if (value && existingNames.includes(value) && value !== initialValues?.name) {
                  return Promise.reject(new Error("该名称已存在"));
                }
                return Promise.resolve();
              },
            },
          ]}
        >
          <Input placeholder="例如: openai, anthropic, deepseek" />
        </Form.Item>

        <Form.Item
          label="Base URL"
          name="base_url"
          rules={[{ required: true, message: "请输入 Base URL" }]}
          tooltip="Provider 的 API 地址，不含具体路径"
        >
          <Input placeholder="https://api.openai.com/v1" />
        </Form.Item>

        <Form.Item
          label="API Key"
          name="api_key"
          dependencies={["format"]}
          rules={[
            ({ getFieldValue }) => ({
              validator: (_, value) => {
                if (getFieldValue("format") !== "kiro" && !value) {
                  return Promise.reject(new Error("请输入 API Key"));
                }
                return Promise.resolve();
              },
            }),
          ]}
        >
          <Input.Password placeholder={isKiro ? "Kiro 认证信息在下方填写" : "sk-..."} />
        </Form.Item>

        <Form.Item
          label="默认模型"
          name="model"
          rules={[{ required: true, message: "请输入模型名称" }]}
          tooltip="未匹配任何模型路由时使用的模型"
        >
          <Input placeholder="gpt-4o" />
        </Form.Item>

        <Form.Item
          label="API 格式"
          name="format"
          rules={[{ required: true, message: "请选择格式" }]}
          tooltip="决定代理如何与此 Provider 通信"
        >
          <Select
            placeholder="选择格式"
            options={[
              { value: "openai", label: "OpenAI" },
              { value: "anthropic", label: "Anthropic" },
              { value: "kiro", label: "Kiro / Amazon Q Developer" },
            ]}
          />
        </Form.Item>
        <Alert
          type="info"
          showIcon={false}
          style={{ marginTop: -8 }}
          message={
            <Text type="secondary" style={{ fontSize: 12 }}>
              大多数 Provider（OpenAI、DeepSeek、Gemini、Azure 等）都使用 OpenAI 格式。
              直连 Anthropic 官方 API 时选 Anthropic 格式；Kiro 使用独立认证设置。
            </Text>
          }
        />
      </Card>

      {isKiro && (
        <Collapse
          defaultActiveKey={["kiro"]}
          style={{ marginBottom: 16 }}
          items={[
            {
              key: "kiro",
              label: "Kiro 设置",
              children: (
                <>
                  <Form.Item
                    label="认证方式"
                    name={["kiro_config", "auth_method"]}
                    rules={[{ required: true, message: "请选择认证方式" }]}
                  >
                    <Select
                      style={{ width: 220 }}
                      options={[
                        { value: "social", label: "Social / Refresh Token" },
                        { value: "idc", label: "IAM Identity Center" },
                        { value: "api_key", label: "API Key" },
                      ]}
                    />
                  </Form.Item>

                  <Form.Item
                    label={kiroAuthMethod === "api_key" ? "Access Token / API Key" : "Refresh Token"}
                    name={["kiro_config", "refresh_token"]}
                    tooltip={kiroAuthMethod === "api_key" ? "api_key 模式下会作为 Bearer token 使用" : undefined}
                  >
                    <Input.Password placeholder={kiroAuthMethod === "api_key" ? "eyJ..." : "refresh token"} />
                  </Form.Item>

                  {kiroAuthMethod === "idc" && (
                    <>
                      <Form.Item label="Client ID" name={["kiro_config", "client_id"]}>
                        <Input placeholder="IAM IdC client id" />
                      </Form.Item>
                      <Form.Item label="Client Secret" name={["kiro_config", "client_secret"]}>
                        <Input.Password placeholder="IAM IdC client secret" />
                      </Form.Item>
                      <Form.Item label="Profile ARN" name={["kiro_config", "profile_arn"]}>
                        <Input placeholder="arn:aws:..." />
                      </Form.Item>
                    </>
                  )}

                  <Space size="middle" wrap>
                    <Form.Item
                      label="Region"
                      name={["kiro_config", "region"]}
                      rules={[{ required: true, message: "请输入 Region" }]}
                    >
                      <Input placeholder="us-east-1" style={{ width: 180 }} />
                    </Form.Item>
                    <Form.Item label="API Region" name={["kiro_config", "api_region"]}>
                      <Input placeholder="默认同 Region" style={{ width: 180 }} />
                    </Form.Item>
                    <Form.Item label="Kiro Version" name={["kiro_config", "kiro_version"]}>
                      <Input placeholder="0.11.107" style={{ width: 180 }} />
                    </Form.Item>
                  </Space>

                  <Form.Item label="Proxy URL" name={["kiro_config", "proxy_url"]}>
                    <Input placeholder="http://127.0.0.1:7890 或 socks5://127.0.0.1:7890" />
                  </Form.Item>

                  <Space size="middle" wrap>
                    <Form.Item label="Thinking 模式" name={["kiro_config", "thinking_mode"]}>
                      <Select
                        style={{ width: 220 }}
                        options={[
                          { value: "as_reasoning_content", label: "Reasoning Content" },
                          { value: "remove", label: "移除" },
                          { value: "pass", label: "保留标签" },
                          { value: "strip_tags", label: "去标签保留内容" },
                        ]}
                      />
                    </Form.Item>
                    <Form.Item label="首选端点" name={["kiro_config", "preferred_endpoint"]}>
                      <Select
                        style={{ width: 180 }}
                        options={[
                          { value: "auto", label: "Auto" },
                          { value: "kiro", label: "Kiro IDE" },
                          { value: "codewhisperer", label: "CodeWhisperer" },
                          { value: "amazonq", label: "AmazonQ" },
                        ]}
                      />
                    </Form.Item>
                    <Form.Item
                      label="429 降级"
                      name={["kiro_config", "endpoint_fallback"]}
                      valuePropName="checked"
                    >
                      <Switch />
                    </Form.Item>
                  </Space>

                  <Space size="middle" wrap>
                    <Form.Item
                      label="Web Search"
                      name={["kiro_config", "web_search_enabled"]}
                      valuePropName="checked"
                    >
                      <Switch />
                    </Form.Item>
                    <Form.Item
                      label="Agentic Prompt"
                      name={["kiro_config", "agentic_prompt_injection"]}
                      valuePropName="checked"
                    >
                      <Switch />
                    </Form.Item>
                  </Space>

                  <Space size="middle" wrap>
                    <Form.Item label="首 Token 超时" name={["kiro_config", "first_token_timeout"]}>
                      <InputNumber min={1} addonAfter="秒" style={{ width: 140 }} />
                    </Form.Item>
                    <Form.Item label="流式读取超时" name={["kiro_config", "streaming_read_timeout"]}>
                      <InputNumber min={1} addonAfter="秒" style={{ width: 140 }} />
                    </Form.Item>
                    <Form.Item label="首 Token 重试" name={["kiro_config", "first_token_max_retries"]}>
                      <InputNumber min={0} style={{ width: 120 }} />
                    </Form.Item>
                    <Form.Item label="配额冷却" name={["kiro_config", "quota_cooldown_secs"]}>
                      <InputNumber min={0} addonAfter="秒" style={{ width: 140 }} />
                    </Form.Item>
                  </Space>
                </>
              ),
            },
          ]}
        />
      )}

      <Collapse
        style={{ marginBottom: 16 }}
        items={[
          {
            key: "quirks",
            label: "高级设置（Provider 兼容性）",
            children: (
              <>
                <Alert
                  type="info"
                  showIcon={false}
                  style={{ marginBottom: 16 }}
                  message={
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      这些选项用于处理不同 Provider 的 API 差异。使用模板创建时会自动配置，通常不需要手动修改。
                    </Text>
                  }
                />
                <Form.Item
                  label="推理内容必须完整"
                  name={["quirks", "reasoning_all_or_nothing"]}
                  valuePropName="checked"
                  tooltip="开启后，历史消息中的 assistant 消息必须全部包含推理内容，否则全部去除。适用于 DeepSeek 等模型。"
                >
                  <Switch />
                </Form.Item>
                <Form.Item
                  label="禁用 JSON Schema 响应"
                  name={["quirks", "no_json_schema"]}
                  valuePropName="checked"
                  tooltip="开启后，将 json_schema 响应格式降级为 json_object。适用于不支持结构化输出的 Provider。"
                >
                  <Switch />
                </Form.Item>
                <Form.Item
                  label="转发推理强度参数"
                  name={["quirks", "supports_reasoning_effort"]}
                  valuePropName="checked"
                  tooltip="开启后，将 thinking/reasoning 配置转换为 reasoning_effort 参数发送给 Provider。适用于 DeepSeek 等支持此参数的模型。"
                >
                  <Switch />
                </Form.Item>
                <Form.Item
                  label="最大推理强度映射值"
                  name={["quirks", "max_reasoning_effort"]}
                  tooltip="Anthropic 的 'max' 或 'adaptive' 推理强度映射到此值。例如 DeepSeek 支持 'max'，OpenAI 最高为 'high'。"
                >
                  <Select
                    style={{ width: 120 }}
                    options={[
                      { value: "high", label: "high" },
                      { value: "max", label: "max" },
                    ]}
                  />
                </Form.Item>
              </>
            ),
          },
        ]}
      />

      <Form.Item>
        <Space>
          <Button type="primary" htmlType="submit">
            {mode === "add" ? "添加" : "保存"}
          </Button>
          <TestConnectionButton form={form} />
          <Button onClick={onCancel}>取消</Button>
        </Space>
      </Form.Item>
    </Form>
  );
}

/** Inline test button that validates form fields and tests the provider. */
function TestConnectionButton({ form }: { form: ReturnType<typeof Form.useForm<ProviderConfig>>[0] }) {
  const [testing, setTesting] = useState(false);
  const providerFormat = Form.useWatch("format", form);

  const handleTest = async () => {
    try {
      // Validate required fields first
      const requiredFields = providerFormat === "kiro"
        ? ["name", "base_url", "model", "format"]
        : ["name", "base_url", "api_key", "model", "format"];
      const values = await form.validateFields(requiredFields);
      const provider: ProviderConfig = {
        ...values,
        quirks: form.getFieldValue("quirks") || {
          reasoning_all_or_nothing: false,
          no_json_schema: false,
          supports_reasoning_effort: false,
          max_reasoning_effort: "high",
        },
      };

      setTesting(true);
      const result = await invoke<TestProviderResult>("test_provider", { provider });
      if (result.success) {
        message.success(
          `连接成功 (${result.latency_ms}ms)${result.model ? ` - 模型: ${result.model}` : ""}`,
          5
        );
      } else {
        message.error({ content: `连接失败: ${result.error}`, duration: 8 });
      }
    } catch (e) {
      // If validation failed, the form will show errors
      if (e && typeof e === "object" && "errorFields" in e) {
        return;
      }
      message.error(`测试失败: ${typeof e === "string" ? e : String(e)}`);
    } finally {
      setTesting(false);
    }
  };

  return (
    <Button
      icon={<ThunderboltOutlined />}
      loading={testing}
      onClick={handleTest}
      disabled={providerFormat === "kiro"}
    >
      {providerFormat === "kiro" ? "在 Kiro 面板测试" : "测试连接"}
    </Button>
  );
}
