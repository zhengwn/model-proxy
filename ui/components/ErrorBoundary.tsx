import { Component, type ErrorInfo, type ReactNode } from "react";
import { Alert, Button } from "antd";
import { useLocale } from "../i18n";

interface Props {
  children: ReactNode;
}

interface State {
  hasError: boolean;
  error: Error | null;
}

function ErrorFallback({ error, onReset }: { error: Error | null; onReset: () => void }) {
  const { t } = useLocale();
  return (
    <div style={{ padding: 48 }}>
      <Alert
        message={t("error.appError")}
        description={
          <div>
            <p>{error?.message || t("error.unknown")}</p>
            <Button type="primary" onClick={onReset}>
              {t("common.retry")}
            </Button>
          </div>
        }
        type="error"
        showIcon
      />
    </div>
  );
}

export class ErrorBoundary extends Component<Props, State> {
  constructor(props: Props) {
    super(props);
    this.state = { hasError: false, error: null };
  }

  static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error };
  }

  componentDidCatch(error: Error, errorInfo: ErrorInfo) {
    console.error("[ErrorBoundary] Uncaught error:", error, errorInfo);
  }

  handleReset = () => {
    this.setState({ hasError: false, error: null });
  };

  render() {
    if (this.state.hasError) {
      return <ErrorFallback error={this.state.error} onReset={this.handleReset} />;
    }
    return this.props.children;
  }
}
