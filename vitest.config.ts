import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import path from "path";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "ui"),
    },
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./ui/__tests__/setup.ts"],
    include: ["ui/**/*.{test,spec}.{ts,tsx}"],
    exclude: ["node_modules", "dist", "target", "reference"],
  },
});
