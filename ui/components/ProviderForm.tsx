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
import { useLocale } from "../i18n";

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
    label: "custom",
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
  const { t } = useLocale();
  const [form] = Form.useForm<ProviderConfig>();

  // Compute the initial form values with useMemo to avoid recalculation on every render
  const computedInitialValues = useMemo(() => {
    if (initialValues) return initialValues;
    if (defaultTemplate) {
      const template = PROVIDER_TEMPLATES.find((t) => t.name === defaultTemplate);
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

  const handleTemplateSelect = (templateName: string) => {
    const template = PROVIDER_TEMPLATES.find((t) => t.name === templateName);
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
      <Card title={t("providerForm.basicInfo")} style={{ marginBottom: 16 }}>
        {mode === "add" && (
          <Form.Item label={t("providerForm.fromTemplate")}>
            <Select
              placeholder={t("providerForm.templatePlaceholder")}
              defaultValue={defaultTemplate}
              allowClear
              onChange={handleTemplateSelect}
              options={PROVIDER_TEMPLATES.map((tpl) => ({
                value: tpl.name,
                label: tpl.name === "" ? t("template.custom") : tpl.label,
              }))}
              style={{ width: 240 }}
            />
            <div style={{ marginTop: 4 }}>
              <Text type="secondary" style={{ fontSize: 12 }}>
                {t("providerForm.templateHint")}
              </Text>
            </div>
          </Form.Item>
        )}
        <Form.Item
          label={t("providerForm.name")}
          name="name"
          rules={[
            { required: true, message: t("providerForm.nameRequired") },
            { max: 64, message: t("providerForm.nameMaxLen") },
            {
              validator: (_, value) => {
                if (value && existingNames.includes(value) && value !== initialValues?.name) {
                  return Promise.reject(new Error(t("providerForm.nameExists")));
                }
                return Promise.resolve();
              },
            },
          ]}
        >
          <Input placeholder={t("providerForm.namePlaceholder")} />
        </Form.Item>

        <Form.Item
          label="Base URL"
          name="base_url"
          rules={[{ required: true, message: t("providerForm.baseUrlRequired") }]}
          tooltip={t("providerForm.baseUrlTooltip")}
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
                  return Promise.reject(new Error(t("providerForm.apiKeyRequired")));
                }
                return Promise.resolve();
              },
            }),
          ]}
        >
          <Input.Password placeholder={isKiro ? t("providerForm.kiroAuthPlaceholder") : "sk-..."} />
        </Form.Item>

        <Form.Item
          label={t("providerForm.defaultModel")}
          name="model"
          rules={[{ required: true, message: t("providerForm.modelRequired") }]}
          tooltip={t("providerForm.modelTooltip")}
        >
          <Input placeholder="gpt-4o" />
        </Form.Item>

        <Form.Item
          label={t("providerForm.format")}
          name="format"
          rules={[{ required: true, message: t("providerForm.formatRequired") }]}
          tooltip={t("providerForm.formatTooltip")}
        >
          <Select
            placeholder={t("providerForm.formatPlaceholder")}
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
          title={
            <Text type="secondary" style={{ fontSize: 12 }}>
              {t("providerForm.formatHint1")}
              <br />
              {t("providerForm.formatHint2")}
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
              label: t("providerForm.kiroSettings"),
              children: (
                <>
                  <Form.Item
                    label={t("providerForm.kiroAuthMethod")}
                    name={["kiro_config", "auth_method"]}
                    rules={[{ required: true, message: t("providerForm.kiroAuthMethodRequired") }]}
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
                    tooltip={kiroAuthMethod === "api_key" ? t("providerForm.bearerTokenHint") : undefined}
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
                      rules={[{ required: true, message: t("providerForm.regionRequired") }]}
                    >
                      <Input placeholder="us-east-1" style={{ width: 180 }} />
                    </Form.Item>
                    <Form.Item label="API Region" name={["kiro_config", "api_region"]}>
                      <Input placeholder={t("providerForm.apiRegionPlaceholder")} style={{ width: 180 }} />
                    </Form.Item>
                    <Form.Item label="Kiro Version" name={["kiro_config", "kiro_version"]}>
                      <Input placeholder="0.11.107" style={{ width: 180 }} />
                    </Form.Item>
                  </Space>

                  <Form.Item label="Proxy URL" name={["kiro_config", "proxy_url"]}>
                    <Input placeholder={t("providerForm.proxyPlaceholder")} />
                  </Form.Item>

                  <Space size="middle" wrap>
                    <Form.Item label={t("providerForm.thinkingMode")} name={["kiro_config", "thinking_mode"]}>
                      <Select
                        style={{ width: 220 }}
                        options={[
                          { value: "as_reasoning_content", label: "Reasoning Content" },
                          { value: "remove", label: t("providerForm.thinkingRemove") },
                          { value: "pass", label: t("providerForm.thinkingPass") },
                          { value: "strip_tags", label: t("providerForm.thinkingStrip") },
                        ]}
                      />
                    </Form.Item>
                    <Form.Item label={t("providerForm.preferredEndpoint")} name={["kiro_config", "preferred_endpoint"]}>
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
                      label={t("providerForm.fallback429")}
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
                    <Form.Item label={t("providerForm.firstTokenTimeout")} name={["kiro_config", "first_token_timeout"]}>
                      <InputNumber min={1} addonAfter={t("providerForm.seconds")} style={{ width: 140 }} />
                    </Form.Item>
                    <Form.Item label={t("providerForm.streamTimeout")} name={["kiro_config", "streaming_read_timeout"]}>
                      <InputNumber min={1} addonAfter={t("providerForm.seconds")} style={{ width: 140 }} />
                    </Form.Item>
                    <Form.Item label={t("providerForm.firstTokenRetry")} name={["kiro_config", "first_token_max_retries"]}>
                      <InputNumber min={0} style={{ width: 120 }} />
                    </Form.Item>
                    <Form.Item label={t("providerForm.quotaCooldown")} name={["kiro_config", "quota_cooldown_secs"]}>
                      <InputNumber min={0} addonAfter={t("providerForm.seconds")} style={{ width: 140 }} />
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
            label: t("providerForm.advancedSettings"),
            children: (
              <>
                <Alert
                  type="info"
                  showIcon={false}
                  style={{ marginBottom: 16 }}
                  title={
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      {t("providerForm.advancedSettingsHint")}
                    </Text>
                  }
                />
                <Form.Item
                  label={t("providerForm.reasoningAllOrNothing")}
                  name={["quirks", "reasoning_all_or_nothing"]}
                  valuePropName="checked"
                  tooltip={t("providerForm.reasoningAllOrNothingTip")}
                >
                  <Switch />
                </Form.Item>
                <Form.Item
                  label={t("providerForm.noJsonSchema")}
                  name={["quirks", "no_json_schema"]}
                  valuePropName="checked"
                  tooltip={t("providerForm.noJsonSchemaTip")}
                >
                  <Switch />
                </Form.Item>
                <Form.Item
                  label={t("providerForm.supportsReasoningEffort")}
                  name={["quirks", "supports_reasoning_effort"]}
                  valuePropName="checked"
                  tooltip={t("providerForm.supportsReasoningEffortTip")}
                >
                  <Switch />
                </Form.Item>
                <Form.Item
                  label={t("providerForm.maxReasoningEffort")}
                  name={["quirks", "max_reasoning_effort"]}
                  tooltip={t("providerForm.maxReasoningEffortTip")}
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
            {mode === "add" ? t("common.add") : t("common.save")}
          </Button>
          <TestConnectionButton form={form} />
          <Button onClick={onCancel}>{t("common.cancel")}</Button>
        </Space>
      </Form.Item>
    </Form>
  );
}

/** Inline test button that validates form fields and tests the provider. */
function TestConnectionButton({ form }: { form: ReturnType<typeof Form.useForm<ProviderConfig>>[0] }) {
  const { t } = useLocale();
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
          t("providerForm.connectSuccess", { latency: result.latency_ms, model: result.model ? ` - ${t("provider.model", { model: result.model })}` : "" }),
          5
        );
      } else {
        message.error({ content: t("providerForm.connectFailed", { error: result.error ?? "" }), duration: 8 });
      }
    } catch (e) {
      // If validation failed, the form will show errors
      if (e && typeof e === "object" && "errorFields" in e) {
        return;
      }
      message.error(t("providerForm.testFailed", { error: typeof e === "string" ? e : String(e) }));
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
      {providerFormat === "kiro" ? t("provider.testInKiro") : t("provider.testConnection")}
    </Button>
  );
}
