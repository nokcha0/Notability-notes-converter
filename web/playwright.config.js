import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  testMatch: "*.spec.js",
  timeout: 90_000,
  expect: { timeout: 20_000 },
  workers: 1,
  use: {
    baseURL: "http://127.0.0.1:4173/web/dist/",
    headless: true,
    launchOptions: process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {},
  },
  webServer: {
    command: "python3 tests/fixtures.py && cargo run --locked --manifest-path ../Cargo.toml -- --input ../input --output ../target/browser-reference && python3 -m http.server 4173 --bind 127.0.0.1 --directory ..",
    url: "http://127.0.0.1:4173/web/dist/",
    reuseExistingServer: !process.env.CI,
  },
});
