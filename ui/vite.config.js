import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import pkg from "./package.json";

// Tauri expects a fixed port the devUrl in tauri.conf.json points to.
// The server must refuse to auto-change the port (strictPort) so that
// Tauri doesn't end up pointing at a stale URL.
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "127.0.0.1",
  },
  envPrefix: ["VITE_", "TAURI_"],
  // Inject the UI package version as a build-time constant so the
  // topbar can show it next to the brand. Bumping the workspace
  // version flows through automatically without a string edit in
  // the template.
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  build: {
    target: "es2021",
    minify: "esbuild",
    sourcemap: true,
  },
});
