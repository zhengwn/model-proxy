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
import { useLocale } from "../i18n";

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

const REASONING_VALUES = ["", "low", "medium", "high", "max"];

export function ModelRoutesEditor() {
  const { t } = useLocale();
  const REASONING_OPTIONS = [
    { value: "", label: t("routes.notSet") },
    ...REASONING_VALUES.filter((v) => v !== "").map((v) => ({ value: v, label: v })),
  ];
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
      message.success(t("routes.saved"));
    } catch (e) {
      message.error(t("common.saveFailed", { error: typeof e === "string" ? e : String(e) }));
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
      <Spin tip={t("routes.loading")} style={{ display: "block", marginTop: 48 }} />
    );
  }

  if (error) {
    return (
      <Card>
        <Alert
          message={t("routes.loadFailed")}
          description={error}
          type="error"
          showIcon
          action={
            <Button size="small" onClick={loadRoutes}>
              {t("common.retry")}
            </Button>
          }
        />
      </Card>
    );
  }

  return (
    <div style={{ maxWidth: 780 }}>
      <Card
        size="small"
        title={t("routes.routeRules")}
        style={{ marginBottom: 16 }}
        extra={
          <Tooltip title={t("routes.routeOrderTip")}>
            <QuestionCircleOutlined />
          </Tooltip>
        }
      >
        {/* Explanation */}
        <Alert
          type="info"
          showIcon={false}
          style={{ marginBottom: 16 }}
          message={
            <div>
              <Paragraph style={{ margin: 0 }}>
                <strong>{t("routes.explanation")}</strong>{t("routes.explanationDesc")}
              </Paragraph>
              <Paragraph style={{ margin: "8px 0 0 0" }}>
                <strong>{t("routes.matchMethod")}</strong>{t("routes.matchDesc")}
              </Paragraph>
            </div>
          }
        />
        {routes.length === 0 && (
          <Text type="secondary" style={{ display: "block", marginBottom: 12 }}>
            {t("routes.noRules")}
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
              border: "1px solid var(--border-color, #434343)",
              borderRadius: 6,
            }}
          >
            <Tooltip title={t("routes.matchTooltip")}>
              <Input
                addonBefore={t("routes.contains")}
                placeholder="sonnet"
                value={route.match}
                onChange={(e) => handleChange(index, "match", e.target.value)}
                style={{ width: 180 }}
              />
            </Tooltip>

            <ArrowRightOutlined style={{ color: "#999", flexShrink: 0 }} />

            <Tooltip title={t("routes.forwardTooltip")}>
              <Input
                addonBefore={t("routes.forwardTo")}
                placeholder="deepseek-chat"
                value={route.target}
                onChange={(e) => handleChange(index, "target", e.target.value)}
                style={{ width: 220 }}
              />
            </Tooltip>

            <Tooltip title={t("routes.effortTooltip")}>
              <Select
                placeholder={t("routes.reasoningEffort")}
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
          {t("routes.addRule")}
        </Button>
      </Card>

      {/* Quick templates */}
      <Card size="small" title={t("routes.quickAdd")} style={{ marginBottom: 16 }}>
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
        {t("routes.saveRoutes")}
      </Button>
    </div>
  );
}
