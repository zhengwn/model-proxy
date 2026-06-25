import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";

export default defineConfig({
  plugins: [react()],
  root: ".",
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "ui"),
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  // Pin dependency scanning to the real app entry. Otherwise vite globs every
  // *.html under the root and crawls into the vendored projects in reference/,
  // failing to resolve their (uninstalled) dependencies.
  optimizeDeps: {
    entries: ["index.html"],
  },
  server: {
    port: 5173,
    strictPort: true,
  },
});
