import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ProviderManager } from "../ProviderManager";
import { LocaleProvider } from "../../i18n";
import { ProvidersProvider } from "../../hooks/useProviders";
import type { ProvidersInfo } from "../../types";

const renderManager = () =>
  render(
    <LocaleProvider>
      <ProvidersProvider>
        <ProviderManager />
      </ProvidersProvider>
    </LocaleProvider>
  );

// Mock Tauri invoke
const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

const mockProvidersInfo: ProvidersInfo = {
  providers: [
    {
      name: "openai",
      base_url: "https://api.openai.com/v1",
      api_key: "sk-openai",
      model: "gpt-4o",
      format: "openai",
      quirks: {
        reasoning_all_or_nothing: false,
        no_json_schema: false,
        supports_reasoning_effort: false,
        max_reasoning_effort: "high",
      },
    },
    {
      name: "anthropic",
      base_url: "https://api.anthropic.com",
      api_key: "sk-ant",
      model: "claude-sonnet-4-20250514",
      format: "anthropic",
      quirks: {
        reasoning_all_or_nothing: false,
        no_json_schema: false,
        supports_reasoning_effort: false,
        max_reasoning_effort: "high",
      },
    },
  ],
  active_provider: "openai",
};

describe("ProviderManager", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("shows loading spinner initially", () => {
    mockInvoke.mockReturnValue(new Promise(() => {})); // Never resolves
    renderManager();

    // Ant Design Spin renders with aria-busy="true" and role="status" via aria-live="polite"
    const spinner = document.querySelector(".ant-spin-spinning");
    expect(spinner).toBeInTheDocument();
  });

  it("shows error alert on load failure", async () => {
    mockInvoke.mockRejectedValue("Network error");
    renderManager();

    await waitFor(() => {
      expect(screen.getByText("Load Failed")).toBeInTheDocument();
    });

    expect(screen.getByText("Network error")).toBeInTheDocument();
  });

  it("renders provider list after successful load", async () => {
    mockInvoke.mockResolvedValue(mockProvidersInfo);
    renderManager();

    await waitFor(() => {
      // "openai" appears as both name and format tag, use getAllByText
      expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    });

    // "anthropic" also appears as both name and format tag
    expect(screen.getAllByText("anthropic").length).toBeGreaterThanOrEqual(1);
    // Active provider tag
    expect(screen.getByText("Active")).toBeInTheDocument();
  });

  it("switch button triggers IPC and updates state", async () => {
    const user = userEvent.setup();
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === "get_providers") return Promise.resolve(mockProvidersInfo);
      if (cmd === "switch_provider") return Promise.resolve();
      return Promise.resolve();
    });

    renderManager();

    await waitFor(() => {
      expect(screen.getAllByText("anthropic").length).toBeGreaterThanOrEqual(1);
    });

    // The switch button has a "swap" icon aria-label and text "切换"
    const switchButton = screen.getByRole("button", { name: /Switch/ });
    await user.click(switchButton);

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith("switch_provider", {
        name: "anthropic",
      });
    });
  });

  it("delete button shows confirmation modal for non-active provider", async () => {
    const user = userEvent.setup();
    mockInvoke.mockResolvedValue(mockProvidersInfo);

    renderManager();

    await waitFor(() => {
      expect(screen.getAllByText("anthropic").length).toBeGreaterThanOrEqual(1);
    });

    // Find delete buttons by their aria-label
    const deleteButtons = screen.getAllByRole("button", { name: /delete/i });
    // Click the delete button for the non-active provider (anthropic - second one)
    await user.click(deleteButtons[1]);

    await waitFor(() => {
      // antd >=5.29 renders the Modal.confirm title in more than one node
      // (visible + a11y), so assert on the unique content message instead.
      expect(
        screen.getByText('Delete Provider "anthropic"?')
      ).toBeInTheDocument();
    });

    expect(screen.getAllByText("Confirm Delete").length).toBeGreaterThanOrEqual(1);
  });

  // Modal.warning does not render in jsdom test environment
  it.skip("delete active provider shows warning modal", async () => {
    const user = userEvent.setup();
    mockInvoke.mockResolvedValue(mockProvidersInfo);

    renderManager();

    await waitFor(() => {
      expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    });

    // Click the delete button for the active provider (openai - first one)
    const deleteButtons = screen.getAllByRole("button", { name: /delete/i });
    await user.click(deleteButtons[0]);

    await waitFor(() => {
      expect(screen.getByText("Cannot delete the active Provider. Please switch to another Provider.")).toBeInTheDocument();
    }, { timeout: 5000 });

    expect(
      screen.getByText(
        "Cannot delete the active Provider. Please switch to another Provider."
      )
    ).toBeInTheDocument();
  });

  it("shows retry button on error", async () => {
    mockInvoke.mockRejectedValue("Connection failed");
    renderManager();

    await waitFor(() => {
      expect(screen.getByText("Load Failed")).toBeInTheDocument();
    });

    // Ant Design Button renders Chinese text with spaces between chars
    // Use a role-based query with regex
    expect(screen.getByRole("button", { name: /Retry/ })).toBeInTheDocument();
  });

  it("retry button reloads providers", async () => {
    const user = userEvent.setup();
    mockInvoke
      .mockRejectedValueOnce("Connection failed")
      .mockResolvedValueOnce(mockProvidersInfo);

    renderManager();

    await waitFor(() => {
      expect(screen.getByText("Load Failed")).toBeInTheDocument();
    });

    const retryButton = screen.getByRole("button", { name: /Retry/ });
    await user.click(retryButton);

    await waitFor(() => {
      // After retry, provider list should be visible
      expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    });
  });
});
