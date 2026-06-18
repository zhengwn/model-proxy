import { useEffect, useMemo, useRef, useState } from "react";
import {
  Button,
  Table,
  Space,
  Tag,
  Select,
  Input,
  Collapse,
  Form,
  Switch,
  InputNumber,
  message,
  Tooltip,
} from "antd";
import { DeleteOutlined, SearchOutlined, SettingOutlined } from "@ant-design/icons";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import type { LogEntry, LoggingConfig } from "../types";
import type { ColumnsType } from "antd/es/table";
import { filterLogEntries, getUniqueProviders, type StatusFilter } from "../utils/logFilters";
import {
  MAX_BODY_BYTES_MIN,
  MAX_BODY_BYTES_MAX,
  RETENTION_DAYS_MIN,
  RETENTION_DAYS_MAX,
} from "../utils/loggingValidation";
import { useLocale } from "../i18n";
import type { Messages } from "../i18n/zh";

const MAX_LOG_ENTRIES = 100;

function summarizeErrorMessage(message?: string): string {
  if (!message) return "-";
  try {
    const parsed = JSON.parse(message);
    const nestedMessage =
      parsed?.error?.message ?? parsed?.message ?? parsed?.error ?? message;
    if (typeof nestedMessage === "string") {
      return nestedMessage;
    }
  } catch {
    // Fall through to the plain-text cleanup below.
  }

  const titleMatch = message.match(/<title[^>]*>(.*?)<\/title>/i);
  const cleaned = (titleMatch?.[1] ?? message)
    .replace(/<[^>]+>/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return cleaned.length > 180 ? `${cleaned.slice(0, 177)}...` : cleaned;
}

function getColumns(t: (key: keyof Messages, params?: Record<string, string | number>) => string): ColumnsType<LogEntry> {
  return [
    {
      title: t("log.time"),
      dataIndex: "timestamp",
      key: "timestamp",
      width: 118,
      render: (val: string) => {
        try {
          return new Date(val).toLocaleTimeString();
        } catch {
          return val;
        }
      },
    },
    {
      title: t("log.method"),
      dataIndex: "method",
      key: "method",
      width: 82,
      render: (method: string) => {
        const color = method === "POST" ? "blue" : method === "GET" ? "green" : "default";
        return <Tag color={color}>{method}</Tag>;
      },
    },
    {
      title: t("log.path"),
      dataIndex: "path",
      key: "path",
      width: 132,
      ellipsis: true,
    },
    {
      title: "Provider",
      dataIndex: "provider",
      key: "provider",
      width: 170,
      ellipsis: true,
    },
    {
      title: t("log.model"),
      dataIndex: "model",
      key: "model",
      width: 360,
      ellipsis: true,
      render: (_: string, record: LogEntry) => {
        const requested = record.requested_model;
        const actual = record.model;
        if (requested && requested !== actual && actual) {
          return (
            <span>
              <span style={{ opacity: 0.6 }}>{requested}</span>
              <span style={{ margin: "0 4px" }}>→</span>
              <span>{actual}</span>
            </span>
          );
        }
        return actual || requested || "-";
      },
    },
    {
      title: t("log.statusCode"),
      dataIndex: "status",
      key: "status",
      width: 92,
      render: (status: number) => {
        const color = status >= 500 ? "red" : status >= 400 ? "orange" : "green";
        return <Tag color={color}>{status}</Tag>;
      },
    },
    {
      title: t("log.proxy"),
      dataIndex: "proxy_overhead_ms",
      key: "proxy_overhead_ms",
      width: 82,
      render: (ms?: number) => ms != null ? `${ms}ms` : "-",
    },
    {
      title: t("log.firstToken"),
      dataIndex: "ttft_ms",
      key: "ttft_ms",
      width: 96,
      render: (ms?: number) => ms != null ? `${ms}ms` : "-",
    },
    {
      title: t("log.transfer"),
      dataIndex: "duration_ms",
      key: "transfer_ms",
      width: 112,
      render: (_: number, record: LogEntry) => {
        if (record.ttft_ms != null && record.proxy_overhead_ms != null) {
          const transfer = record.duration_ms - record.proxy_overhead_ms - record.ttft_ms;
          return transfer >= 0 ? `${transfer}ms` : "-";
        }
        return "-";
      },
    },
    {
      title: t("log.error"),
      dataIndex: "error_message",
      key: "error_message",
      width: 280,
      ellipsis: true,
      render: (message?: string) => {
        if (!message) return "-";
        const summary = summarizeErrorMessage(message);
        return (
          <Tooltip title={message}>
            <span style={{ color: "#faad14" }}>{summary}</span>
          </Tooltip>
        );
      },
    },
  ];
}

function getStatusFilterOptions(t: (key: keyof Messages, params?: Record<string, string | number>) => string) {
  return [
    { label: t("log.all"), value: "all" as const },
    { label: "2xx", value: "2xx" as const },
    { label: "4xx", value: "4xx" as const },
    { label: "5xx", value: "5xx" as const },
  ];
}

const DEFAULT_LOGGING: LoggingConfig = {
  enabled: true,
  level: "all",
  record_body: false,
  max_body_bytes: 4096,
  retention_days: 7,
};

function LogSettings() {
  const [form] = Form.useForm<LoggingConfig>();
  const [loading, setLoading] = useState(true);
  const { t } = useLocale();

  useEffect(() => {
    (async () => {
      try {
        const config = await invoke<{ logging?: LoggingConfig }>("get_config");
        form.setFieldsValue({ ...DEFAULT_LOGGING, ...config.logging });
      } catch {
        form.setFieldsValue(DEFAULT_LOGGING);
      } finally {
        setLoading(false);
      }
    })();
  }, [form]);

  const handleSave = async () => {
    try {
      const values = await form.validateFields();
      // Load full config, update logging section, save back
      const config = await invoke<Record<string, unknown>>("get_config");
      await invoke<void>("save_config", {
        config: { ...config, logging: values },
      });
      message.success(t("log.settingsSaved"));
    } catch (e) {
      if (e && typeof e === "object" && "errorFields" in e) {
        message.error(t("log.formError"));
      } else {
        message.error(t("common.saveFailed", { error: typeof e === "string" ? e : String(e) }));
      }
    }
  };

  if (loading) return null;

  return (
    <Form form={form} layout="inline" size="small" style={{ marginBottom: 8 }}>
      <Form.Item name="enabled" valuePropName="checked" label={t("log.enabled")}>
        <Switch size="small" />
      </Form.Item>
      <Form.Item name="level" label={t("log.level")}>
        <Select
          style={{ width: 110 }}
          options={[
            { value: "all", label: t("log.allRequests") },
            { value: "errors_only", label: t("log.errorsOnly") },
          ]}
        />
      </Form.Item>
      <Form.Item name="record_body" valuePropName="checked" label={t("log.recordBody")}>
        <Switch size="small" />
      </Form.Item>
      <Form.Item
        name="max_body_bytes"
        label={t("log.maxBytes")}
        rules={[
          {
            type: "number",
            min: MAX_BODY_BYTES_MIN,
            max: MAX_BODY_BYTES_MAX,
          },
        ]}
      >
        <InputNumber
          min={MAX_BODY_BYTES_MIN}
          max={MAX_BODY_BYTES_MAX}
          step={1024}
          style={{ width: 100 }}
        />
      </Form.Item>
      <Form.Item
        name="retention_days"
        label={t("log.retentionDays")}
        rules={[
          {
            type: "number",
            min: RETENTION_DAYS_MIN,
            max: RETENTION_DAYS_MAX,
          },
        ]}
      >
        <InputNumber
          min={RETENTION_DAYS_MIN}
          max={RETENTION_DAYS_MAX}
          style={{ width: 70 }}
        />
      </Form.Item>
      <Form.Item>
        <Button type="primary" size="small" onClick={handleSave}>
          {t("common.save")}
        </Button>
      </Form.Item>
    </Form>
  );
}

function LogViewer() {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [statusFilter, setStatusFilter] = useState<StatusFilter>("all");
  const [providerFilter, setProviderFilter] = useState<string | undefined>(undefined);
  const [keyword, setKeyword] = useState<string>("");
  const tableRef = useRef<HTMLDivElement>(null);
  const { t } = useLocale();

  const columns = useMemo(() => getColumns(t), [t]);
  const statusFilterOptions = useMemo(() => getStatusFilterOptions(t), [t]);

  useEffect(() => {
    const unlisten = listen<LogEntry>("proxy-log", (event) => {
      setLogs((prev) => {
        const updated = [...prev, event.payload];
        if (updated.length > MAX_LOG_ENTRIES) {
          return updated.slice(updated.length - MAX_LOG_ENTRIES);
        }
        return updated;
      });
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const uniqueProviders = useMemo(() => getUniqueProviders(logs), [logs]);

  const filteredLogs = useMemo(
    () => filterLogEntries(logs, statusFilter, providerFilter, keyword),
    [logs, statusFilter, providerFilter, keyword]
  );

  // Auto-scroll to bottom when new logs arrive
  useEffect(() => {
    if (tableRef.current) {
      const tableBody = tableRef.current.querySelector(".ant-table-body");
      if (tableBody) {
        tableBody.scrollTop = tableBody.scrollHeight;
      }
    }
  }, [filteredLogs]);

  const handleClear = () => {
    setLogs([]);
    setStatusFilter("all");
    setProviderFilter(undefined);
    setKeyword("");
  };

  return (
    <div ref={tableRef}>
      <Collapse
        size="small"
        style={{ marginBottom: 16 }}
        items={[
          {
            key: "settings",
            label: (
              <span>
                <SettingOutlined style={{ marginRight: 8 }} />
                {t("log.settings")}
              </span>
            ),
            children: <LogSettings />,
          },
        ]}
      />
      <Space style={{ marginBottom: 16 }} wrap>
        <Button icon={<DeleteOutlined />} onClick={handleClear} disabled={logs.length === 0}>
          {t("log.clearLogs")}
        </Button>
        <Select
          value={statusFilter}
          onChange={setStatusFilter}
          options={statusFilterOptions}
          style={{ width: 100 }}
          size="small"
        />
        <Select
          value={providerFilter}
          onChange={setProviderFilter}
          placeholder="Provider"
          allowClear
          style={{ width: 140 }}
          size="small"
          options={uniqueProviders.map((p) => ({ label: p, value: p }))}
        />
        <Input
          prefix={<SearchOutlined />}
          placeholder={t("log.searchPlaceholder")}
          value={keyword}
          onChange={(e) => setKeyword(e.target.value)}
          allowClear
          style={{ width: 200 }}
          size="small"
        />
        <span style={{ color: "#999", fontSize: 12 }}>
          {t("log.displayCount", { filtered: filteredLogs.length, total: logs.length, max: MAX_LOG_ENTRIES })}
        </span>
      </Space>
      <Table
        columns={columns}
        dataSource={filteredLogs}
        rowKey="id"
        size="small"
        pagination={false}
        tableLayout="fixed"
        scroll={{ x: 1524, y: 400 }}
        locale={{ emptyText: t("log.noLogs") }}
      />
    </div>
  );
}

export default LogViewer;
