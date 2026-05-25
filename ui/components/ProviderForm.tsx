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
} from "antd";
import { ThunderboltOutlined } from "@ant-design/icons";
import { invoke } from "@tauri-apps/api/core";
import type { ProviderConfig, TestProviderResult } from "../types";

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

interface ProviderTemplate {
  label: string;
  name: string;
  base_url: string;
  model: string;
  format: "openai" | "anthropic";
  quirks: typeof defaultQuirks;
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
  }, [form, computedInitialValues]);

  const handleFinish = async (values: ProviderConfig) => {
    const config: ProviderConfig = {
      ...values,
      quirks: {
        ...defaultQuirks,
        ...values.quirks,
      },
    };
    await onSubmit(config);
  };

  const handleTemplateSelect = (templateLabel: string) => {
    const template = PROVIDER_TEMPLATES.find((t) => t.label === templateLabel);
    if (template) {
      form.setFieldsValue({
        name: existingNames.includes(template.name) ? "" : template.name,
        base_url: template.base_url,
        model: template.model,
        format: template.format,
      });
      // Set quirks fields individually to ensure Switch components update correctly
      form.setFieldValue(["quirks", "reasoning_all_or_nothing"], template.quirks.reasoning_all_or_nothing);
      form.setFieldValue(["quirks", "no_json_schema"], template.quirks.no_json_schema);
      form.setFieldValue(["quirks", "supports_reasoning_effort"], template.quirks.supports_reasoning_effort);
      form.setFieldValue(["quirks", "max_reasoning_effort"], template.quirks.max_reasoning_effort);
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
          rules={[{ required: true, message: "请输入 API Key" }]}
        >
          <Input.Password placeholder="sk-..." />
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
              只有直连 Anthropic 官方 API 时才选 Anthropic 格式。
            </Text>
          }
        />
      </Card>

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

  const handleTest = async () => {
    try {
      // Validate required fields first
      const values = await form.validateFields(["name", "base_url", "api_key", "model", "format"]);
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
    >
      测试连接
    </Button>
  );
}
