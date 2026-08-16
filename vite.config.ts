import { defineConfig } from "vite";

// Tauri serves the frontend from a fixed port in dev and from `dist/` in release.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: "127.0.0.1",
    watch: {
      // src-tauri is compiled by cargo, never by vite.
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    // Match the oldest webview we support (WebView2 / WKWebView / WebKitGTK).
    target: ["es2022", "chrome110", "safari15"],
    minify: "esbuild",
    sourcemap: false,
    emptyOutDir: true,
  },
});
