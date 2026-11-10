import { defineConfig } from "@playwright/test";
const port = process.env.PLAYWRIGHT_PORT || "18080";
export default defineConfig({
  testDir: "./tests",
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    viewport: { width: 1440, height: 1000 },
  },
  webServer: {
    command: "npm start",
    url: `http://127.0.0.1:${port}/healthz`,
    env: { PORT: port },
    reuseExistingServer: false,
  },
});
