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
import type { Config } from "../types";

const { Text } = Typography;

export function ConfigEditor() {
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
      const values = await form.validateFields();
      // Preserve existing providers when saving server config
      if (config) {
        values.providers = config.providers;
        values.active_provider = config.active_provider;
        values.model_routes = config.model_routes;
        values.logging = config.logging;
      }
      await saveConfig(values);
      message.success("配置保存成功");
    } catch (e) {
      if (e && typeof e === "object" && "errorFields" in e) {
        message.error("请检查表单中的错误");
      } else {
        message.error(`保存失败: ${typeof e === "string" ? e : String(e)}`);
      }
    }
  };

  if (loading) {
    return <Spin tip="加载配置中..." style={{ display: "block", marginTop: 48 }} />;
  }

  if (error && !config) {
    return (
      <Card>
        <Text type="danger">{error}</Text>
        <br />
        <Button onClick={loadConfig} style={{ marginTop: 12 }}>
          重试
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
          配置文件路径: {configPath}
        </Text>
      )}

      {isNew && (
        <Alert
          message="首次使用"
          description="配置文件尚未创建，请填写以下配置后点击保存。"
          type="info"
          showIcon
          style={{ marginBottom: 16 }}
        />
      )}

      <Card title="服务器设置" size="small" style={{ marginBottom: 16 }}>
        <Form.Item
          label="端口"
          name={["server", "port"]}
          rules={[{ required: true, message: "请输入端口号" }]}
        >
          <InputNumber min={1} max={65535} style={{ width: "100%" }} />
        </Form.Item>
        <Form.Item label="API Key (可选)" name={["server", "api_key"]}>
          <Input.Password placeholder="留空则不启用鉴权" />
        </Form.Item>
      </Card>

      <Form.Item>
        <Button type="primary" onClick={handleSave}>
          保存配置
        </Button>
      </Form.Item>
    </Form>
  );
}
