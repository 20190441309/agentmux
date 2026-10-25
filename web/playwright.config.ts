import { defineConfig } from "@playwright/test";

// End-to-end tests drive a real agentmux-server (with the mock agent) through
// a real browser. Build first: `cargo build --workspace && npm run build`.
export default defineConfig({
  testDir: "e2e",
  timeout: 60_000,
  fullyParallel: false,
  workers: 1,
  reporter: "list",
  use: { trace: "retain-on-failure" },
});
