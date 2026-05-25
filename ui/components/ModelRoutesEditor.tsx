import { useEffect, useState } from "react";
import {
  Button,
  Card,
  Input,
  Select,
  Space,
  Spin,
  Alert,
  message,
  Typography,
  Tooltip,
} from "antd";
import {
  MinusCircleOutlined,
  PlusOutlined,
  ArrowRightOutlined,
  QuestionCircleOutlined,
} from "@ant-design/icons";
import { invoke } from "@tauri-apps/api/core";
import type { ModelRoute } from "../types";

const { Text, Paragraph } = Typography;

interface RouteTemplate {
  label: string;
  match: string;
  target: string;
  reasoning_effort?: string;
}

const ROUTE_TEMPLATES: RouteTemplate[] = [
  {
    label: "Opus → DeepSeek Pro",
    match: "opus",
    target: "deepseek-v4-pro",
    reasoning_effort: "max",
  },
  {
    label: "Sonnet → DeepSeek Pro",
    match: "sonnet",
    target: "deepseek-v4-pro",
    reasoning_effort: "max",
  },
  {
    label: "Haiku → DeepSeek Flash",
    match: "haiku",
    target: "deepseek-v4-flash",
    reasoning_effort: "high",
  },
  {
    label: "Sonnet → GPT-4o",
    match: "sonnet",
    target: "gpt-4o",
  },
  {
    label: "Sonnet → Gemini 2.5 Pro",
    match: "sonnet",
    target: "gemini-2.5-pro",
    reasoning_effort: "high",
  },
];

const REASONING_OPTIONS = [
  { value: "", label: "不设置" },
  { value: "low", label: "low" },
  { value: "medium", label: "medium" },
  { value: "high", label: "high" },
  { value: "max", label: "max" },
];

export function ModelRoutesEditor() {
  const [routes, setRoutes] = useState<ModelRoute[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const loadRoutes = async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await invoke<ModelRoute[]>("get_model_routes");
      if (data.length === 0) {
        // Pre-populate with default routes on first use
        setRoutes([
          { match: "opus", target: "deepseek-v4-pro", reasoning_effort: "max" },
          { match: "sonnet", target: "deepseek-v4-pro", reasoning_effort: "max" },
          { match: "haiku", target: "deepseek-v4-flash", reasoning_effort: "high" },
        ]);
      } else {
        setRoutes(data);
      }
    } catch (e) {
      const errMsg = typeof e === "string" ? e : String(e);
      if (errMsg.includes("不存在")) {
        // Config doesn't exist yet, show defaults
        setRoutes([
          { match: "opus", target: "deepseek-v4-pro", reasoning_effort: "max" },
          { match: "sonnet", target: "deepseek-v4-pro", reasoning_effort: "max" },
          { match: "haiku", target: "deepseek-v4-flash", reasoning_effort: "high" },
        ]);
      } else {
        setError(errMsg);
      }
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    loadRoutes();
  }, []);

  const handleSave = async () => {
    const validRoutes = routes.filter((r) => r.match && r.target);
    try {
      await invoke<void>("save_model_routes", {
        routes: validRoutes.map((r) => ({
          match: r.match,
          target: r.target,
          reasoning_effort: r.reasoning_effort || null,
        })),
      });
      setRoutes(validRoutes);
      message.success("模型路由保存成功");
    } catch (e) {
      message.error(`保存失败: ${typeof e === "string" ? e : String(e)}`);
    }
  };

  const handleAdd = () => {
    setRoutes([...routes, { match: "", target: "", reasoning_effort: "" }]);
  };

  const handleAddTemplate = (template: RouteTemplate) => {
    setRoutes([
      ...routes,
      {
        match: template.match,
        target: template.target,
        reasoning_effort: template.reasoning_effort || "",
      },
    ]);
  };

  const handleRemove = (index: number) => {
    setRoutes(routes.filter((_, i) => i !== index));
  };

  const handleChange = (
    index: number,
    field: keyof ModelRoute,
    value: string
  ) => {
    const updated = [...routes];
    updated[index] = { ...updated[index], [field]: value };
    setRoutes(updated);
  };

  if (loading) {
    return (
      <Spin tip="加载模型路由..." style={{ display: "block", marginTop: 48 }} />
    );
  }

  if (error) {
    return (
      <Card>
        <Alert
          message="加载失败"
          description={error}
          type="error"
          showIcon
          action={
            <Button size="small" onClick={loadRoutes}>
              重试
            </Button>
          }
        />
      </Card>
    );
  }

  return (
    <div style={{ maxWidth: 780 }}>
      {/* Explanation */}
      <Alert
        type="info"
        showIcon={false}
        style={{ marginBottom: 16 }}
        message={
          <div>
            <Paragraph style={{ margin: 0 }}>
              <strong>模型路由的作用：</strong>IDE 发来的请求中会带一个模型名（如{" "}
              <Text code>claude-sonnet-4</Text>
              ），路由规则可以把它映射到你实际想用的模型。
            </Paragraph>
            <Paragraph style={{ margin: "8px 0 0 0" }}>
              <strong>匹配方式：</strong>只要请求的模型名
              <Text strong>包含</Text>「匹配关键词」就会命中（不区分大小写）。
              例如关键词填 <Text code>sonnet</Text>，则{" "}
              <Text code>claude-sonnet-4</Text>、
              <Text code>claude-3-5-sonnet</Text> 都会匹配。
            </Paragraph>
          </div>
        }
      />

      {/* Route list */}
      <Card
        size="small"
        title="路由规则"
        style={{ marginBottom: 16 }}
        extra={
          <Tooltip title="规则按顺序匹配，第一条命中的生效">
            <QuestionCircleOutlined />
          </Tooltip>
        }
      >
        {routes.length === 0 && (
          <Text type="secondary" style={{ display: "block", marginBottom: 12 }}>
            暂无路由规则。未配置路由时，所有请求都使用当前 Provider 的默认模型。
          </Text>
        )}

        {routes.map((route, index) => (
          <div
            key={index}
            style={{
              display: "flex",
              alignItems: "center",
              gap: 8,
              marginBottom: 10,
              padding: "8px 12px",
              border: "1px solid #d9d9d9",
              borderRadius: 6,
            }}
          >
            <Tooltip title="当请求的模型名包含此关键词时触发路由">
              <Input
                addonBefore="包含"
                placeholder="sonnet"
                value={route.match}
                onChange={(e) => handleChange(index, "match", e.target.value)}
                style={{ width: 180 }}
              />
            </Tooltip>

            <ArrowRightOutlined style={{ color: "#999", flexShrink: 0 }} />

            <Tooltip title="实际发送给 Provider 的模型名">
              <Input
                addonBefore="转发到"
                placeholder="deepseek-chat"
                value={route.target}
                onChange={(e) => handleChange(index, "target", e.target.value)}
                style={{ width: 220 }}
              />
            </Tooltip>

            <Tooltip title="覆盖推理强度（仅对支持的 Provider 生效）">
              <Select
                placeholder="推理强度"
                value={route.reasoning_effort || ""}
                onChange={(v) => handleChange(index, "reasoning_effort", v)}
                options={REASONING_OPTIONS}
                style={{ width: 110 }}
                size="middle"
              />
            </Tooltip>

            <Button
              type="text"
              danger
              icon={<MinusCircleOutlined />}
              onClick={() => handleRemove(index)}
              size="small"
            />
          </div>
        ))}

        <Button
          type="dashed"
          onClick={handleAdd}
          block
          icon={<PlusOutlined />}
          style={{ marginTop: 4 }}
        >
          添加规则
        </Button>
      </Card>

      {/* Quick templates */}
      <Card size="small" title="快速添加" style={{ marginBottom: 16 }}>
        <Space wrap>
          {ROUTE_TEMPLATES.map((tpl) => (
            <Button
              key={tpl.label}
              size="small"
              onClick={() => handleAddTemplate(tpl)}
            >
              {tpl.label}
            </Button>
          ))}
        </Space>
      </Card>

      <Button type="primary" onClick={handleSave}>
        保存路由
      </Button>
    </div>
  );
}
