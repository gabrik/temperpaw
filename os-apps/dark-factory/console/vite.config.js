import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The console talks to the temper server directly (no BFF, ADR-0067 D2).
// Dev and preview both proxy /tdata + /auth to the temper server on 3100 so
// the browser session cookie stays same-origin. The console itself is
// served on 8081 (D6).
const TEMPER_TARGET = process.env.TEMPER_URL ?? "http://127.0.0.1:3100";

const apiProxy = {
  target: TEMPER_TARGET,
  changeOrigin: true,
};

export default defineConfig({
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    port: 8081,
    proxy: {
      "/tdata": apiProxy,
      "/auth": apiProxy,
    },
  },
  preview: {
    host: "127.0.0.1",
    port: 8081,
    proxy: {
      "/tdata": apiProxy,
      "/auth": apiProxy,
    },
  },
});
