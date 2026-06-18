import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, fireEvent, act } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ProviderForm } from "../ProviderForm";
import { LocaleProvider } from "../../i18n";
import type { ProviderConfig } from "../../types";

const existingNames = ["openai", "anthropic"];

const mockInitialValues: ProviderConfig = {
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
};

describe("ProviderForm", () => {
  const defaultProps = {
    mode: "add" as const,
    existingNames,
    onSubmit: vi.fn().mockResolvedValue(undefined),
    onCancel: vi.fn(),
  };

  it("in add mode, name field is editable", () => {
    render(<LocaleProvider><ProviderForm {...defaultProps} /></LocaleProvider>);

    const nameInput = document.querySelector("input#name") as HTMLInputElement;
    expect(nameInput).not.toBeNull();
    expect(nameInput).not.toBeDisabled();
  });

  it("in edit mode, name field is editable", () => {
    render(
      <LocaleProvider><ProviderForm
        {...defaultProps}
        mode="edit"
        initialValues={mockInitialValues}
      /></LocaleProvider>
    );

    const nameInput = document.querySelector("input#name") as HTMLInputElement;
    expect(nameInput).not.toBeNull();
    expect(nameInput).not.toBeDisabled();
  });

  it("required field validation shows error on submit attempt", async () => {
    render(<LocaleProvider><ProviderForm {...defaultProps} /></LocaleProvider>);

    // Submit the form using fireEvent on the form element
    const form = document.querySelector("form") as HTMLFormElement;
    await act(async () => {
      fireEvent.submit(form);
    });

    // Antd form validation is async - wait for error state classes
    await waitFor(
      () => {
        const errorItems = document.querySelectorAll(
          ".ant-form-item-has-error"
        );
        // At least the required fields should show errors
        expect(errorItems.length).toBeGreaterThanOrEqual(1);
      },
      { timeout: 3000 }
    );
  });

  it("name uniqueness validation shows error for duplicate names", async () => {
    const user = userEvent.setup();
    render(<LocaleProvider><ProviderForm {...defaultProps} /></LocaleProvider>);

    const nameInput = document.querySelector("input#name") as HTMLInputElement;
    await user.type(nameInput, "openai");

    // Submit the form to trigger validation
    const form = document.querySelector("form") as HTMLFormElement;
    await act(async () => {
      fireEvent.submit(form);
    });

    await waitFor(
      () => {
        expect(screen.getByText("This name already exists")).toBeInTheDocument();
      },
      { timeout: 3000 }
    );
  });

  it("successful submit calls onSubmit with form values", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <LocaleProvider><ProviderForm {...defaultProps} onSubmit={onSubmit} existingNames={[]} /></LocaleProvider>
    );

    // Fill in all required fields
    const nameInput = document.querySelector("input#name") as HTMLInputElement;
    const baseUrlInput = document.querySelector(
      "input#base_url"
    ) as HTMLInputElement;
    const apiKeyInput = document.querySelector(
      "input#api_key"
    ) as HTMLInputElement;
    const modelInput = document.querySelector(
      "input#model"
    ) as HTMLInputElement;

    await user.type(nameInput, "deepseek");
    await user.type(baseUrlInput, "https://api.deepseek.com/v1");
    await user.type(apiKeyInput, "sk-ds-key");
    await user.type(modelInput, "deepseek-chat");

    // Select format - open the dropdown using mouseDown on the format selector
    // (skip the first select which is the template selector)
    const selectInputs = document.querySelectorAll(
      ".ant-select-selector"
    );
    const formatSelect = selectInputs[selectInputs.length - 1] as HTMLElement;
    await act(async () => {
      fireEvent.mouseDown(formatSelect);
    });

    // Wait for dropdown and click option
    await waitFor(() => {
      expect(
        document.querySelector(".ant-select-item")
      ).toBeInTheDocument();
    });

    const option = document.querySelector(
      '.ant-select-item[title="OpenAI"]'
    ) as HTMLElement;
    await act(async () => {
      fireEvent.click(option);
    });

    // Submit the form
    const form = document.querySelector("form") as HTMLFormElement;
    await act(async () => {
      fireEvent.submit(form);
    });

    await waitFor(
      () => {
        expect(onSubmit).toHaveBeenCalledTimes(1);
      },
      { timeout: 3000 }
    );

    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        name: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        api_key: "sk-ds-key",
        model: "deepseek-chat",
        format: "openai",
      })
    );
  });

  it("cancel button calls onCancel", async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    render(<LocaleProvider><ProviderForm {...defaultProps} onCancel={onCancel} /></LocaleProvider>);

    const cancelButton = screen.getByRole("button", { name: /Cancel/ });
    await user.click(cancelButton);

    expect(onCancel).toHaveBeenCalled();
  });
});
