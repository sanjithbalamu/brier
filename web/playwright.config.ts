import { defineConfig, devices } from "@playwright/test";
import { GATEWAY_PORT, WEB_PORT } from "./e2e/stack";

// End-to-end flows against a real exchange: the test-clock gateway (G5) on PostgreSQL
// (BRIER_TEST_DATABASE_URL), seeded with the launch catalogue, and this app built for production.
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 60_000,
  reporter: [["list"]],
  globalSetup: "./e2e/global-setup.ts",
  globalTeardown: "./e2e/global-teardown.ts",
  use: {
    baseURL: `http://127.0.0.1:${WEB_PORT}`,
    trace: "retain-on-failure",
    launchOptions: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH
      ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH }
      : undefined,
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: `npx next build && npx next start --port ${WEB_PORT}`,
    url: `http://127.0.0.1:${WEB_PORT}`,
    timeout: 240_000,
    reuseExistingServer: false,
    env: {
      BRIER_GATEWAY_URL: `http://127.0.0.1:${GATEWAY_PORT}`,
      BRIER_WS_URL: `ws://127.0.0.1:${GATEWAY_PORT}/v1/ws`,
    },
  },
});
