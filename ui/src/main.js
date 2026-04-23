import { createApp } from "vue";
import App from "./App.vue";
import "./style.css";

const app = createApp(App);

// Visible fallback so a crash during render doesn't leave a blank window.
app.config.errorHandler = (err, _instance, info) => {
  console.error("Vue error:", err, info);
  const banner = document.createElement("pre");
  banner.style.cssText = `
    position: fixed; top: 0; left: 0; right: 0; z-index: 99999;
    margin: 0; padding: 1rem; font-family: monospace; font-size: 13px;
    background: #2a0d0d; color: #ff8a8a; border-bottom: 2px solid #ff5d5d;
    white-space: pre-wrap; max-height: 40vh; overflow-y: auto;
  `;
  banner.textContent = `Vue render error (${info}):\n${err?.stack || err}`;
  document.body.appendChild(banner);
};

window.addEventListener("error", (ev) => {
  console.error("window.error:", ev.error || ev.message);
});
window.addEventListener("unhandledrejection", (ev) => {
  console.error("unhandled promise rejection:", ev.reason);
});

app.mount("#app");
