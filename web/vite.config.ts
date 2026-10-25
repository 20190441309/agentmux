import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Dev: `npm run dev` proxies the gateway of a local `agentmux-server --serve --web`.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": "http://127.0.0.1:8787",
      "/ws": { target: "ws://127.0.0.1:8787", ws: true },
    },
  },
  build: { sourcemap: false },
  test: { environment: "node", exclude: ["e2e/**", "node_modules/**"] },
});
