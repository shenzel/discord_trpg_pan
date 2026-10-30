// UI のゴールデン画像テスト。モック（src/mock.ts）の画面を撮って、tests-ui/golden の基準画像と比べる。
import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "tests-ui",
  snapshotPathTemplate: "{testDir}/golden/{arg}{ext}",
  use: {
    baseURL: "http://localhost:1420",
    // インストール済みの Chrome を使う（ブラウザのダウンロードは不要）
    channel: "chrome",
    // アプリのウィンドウと同じ大きさ（tauri.conf.json）
    viewport: { width: 380, height: 640 },
    locale: "ja-JP",
  },
  expect: { toHaveScreenshot: { maxDiffPixels: 50 } },
  webServer: {
    command: "npx vite --port 1420 --strictPort",
    url: "http://localhost:1420",
    reuseExistingServer: true,
  },
});
