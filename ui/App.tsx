import { Layout, Tabs } from "antd";
import type { TabsProps } from "antd";
import StatusPanel from "./components/StatusPanel";
import { ProviderManager } from "./components/ProviderManager";
import { ModelRoutesEditor } from "./components/ModelRoutesEditor";
import LogViewer from "./components/LogViewer";
import { ErrorBoundary } from "./components/ErrorBoundary";

const { Header, Content } = Layout;

const items: TabsProps["items"] = [
  {
    key: "status",
    label: "服务状态",
    children: <StatusPanel />,
  },
  {
    key: "providers",
    label: "Provider 管理",
    children: <ProviderManager />,
  },
  {
    key: "routes",
    label: "模型路由",
    children: <ModelRoutesEditor />,
  },
  {
    key: "logs",
    label: "请求日志",
    children: <LogViewer />,
  },
];

function App() {
  return (
    <ErrorBoundary>
      <Layout style={{ minHeight: "100vh" }}>
        <Header
          style={{
            display: "flex",
            alignItems: "center",
            padding: "0 24px",
          }}
        >
          <h1 style={{ color: "#fff", margin: 0, fontSize: 18 }}>Model Proxy</h1>
        </Header>
        <Content style={{ padding: "24px" }}>
          <Tabs defaultActiveKey="status" items={items} />
        </Content>
      </Layout>
    </ErrorBoundary>
  );
}

export default App;
