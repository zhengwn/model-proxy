import { useEffect } from "react";
import {
  Form,
  Input,
  InputNumber,
  Button,
  Card,
  Spin,
  Alert,
  message,
  Typography,
} from "antd";
import { useConfig } from "../hooks/useConfig";
import { useLocale } from "../i18n";
import type { Config } from "../types";

const { Text } = Typography;

export function ConfigEditor() {
  const { t } = useLocale();
  const { config, loading, error, configPath, isNew, saveConfig, loadConfig } =
    useConfig();
  const [form] = Form.useForm<Config>();

  useEffect(() => {
    if (config) {
      form.setFieldsValue(config);
    }
  }, [config, form]);

  const handleSave = async () => {
    try {
      const rawValues = await form.validateFields();
      const values = { ...rawValues };
      if (config) {
        values.providers = config.providers;
        values.active_provider = config.active_provider;
        values.model_routes = config.model_routes;
        values.logging = config.logging;
      }
      await saveConfig(values);
      message.success(t("config.saved"));
    } catch (e) {
      if (e && typeof e === "object" && "errorFields" in e) {
        message.error(t("config.formError"));
      } else {
        message.error(t("common.saveFailed", { error: typeof e === "string" ? e : String(e) }));
      }
    }
  };

  if (loading) {
    return <Spin tip={t("config.loading")} style={{ display: "block", marginTop: 48 }} />;
  }

  if (error && !config) {
    return (
      <Card>
        <Text type="danger">{error}</Text>
        <br />
        <Button onClick={loadConfig} style={{ marginTop: 12 }}>
          {t("common.retry")}
        </Button>
      </Card>
    );
  }

  return (
    <Form
      form={form}
      layout="vertical"
      autoComplete="off"
      style={{ maxWidth: 720 }}
    >
      {configPath && (
        <Text type="secondary" style={{ display: "block", marginBottom: 16 }}>
          {t("config.configPath", { path: configPath })}
        </Text>
      )}

      {isNew && (
        <Alert
          title={t("config.firstUse")}
          description={t("config.firstUseDesc")}
          type="info"
          showIcon
          style={{ marginBottom: 16 }}
        />
      )}

      <Card title={t("config.serverSettings")} size="small" style={{ marginBottom: 16 }}>
        <Form.Item
          label={t("config.port")}
          name={["server", "port"]}
          rules={[{ required: true, message: t("config.portRequired") }]}
        >
          <InputNumber min={1} max={65535} style={{ width: "100%" }} />
        </Form.Item>
        <Form.Item label={t("config.apiKeyOptional")} name={["server", "api_key"]}>
          <Input.Password placeholder={t("config.apiKeyPlaceholder")} />
        </Form.Item>
      </Card>

      <Form.Item>
        <Button type="primary" onClick={handleSave}>
          {t("config.saveConfig")}
        </Button>
      </Form.Item>
    </Form>
  );
}
