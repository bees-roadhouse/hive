import { defineConfig } from "@playwright/test";

const uiBase = process.env.HIVE_UI_URL ?? "http://127.0.0.1:8091";
const apiBase = process.env.HIVE_API_URL ?? "http://127.0.0.1:7878";

export default defineConfig({
  testDir: "./tests",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: uiBase,
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
  metadata: { apiBase },
});
