import { defineConfig, devices } from "@playwright/test";

// The generated reports need Python and a browser, but no exchange, database or Next server.
export default defineConfig({
  testDir: "./e2e",
  testMatch: "research.spec.ts",
  workers: 1,
  timeout: 60_000,
  reporter: [["list"]],
  use: {
    ...devices["Desktop Chrome"],
    trace: "retain-on-failure",
    launchOptions: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH
      ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH }
      : undefined,
  },
});
