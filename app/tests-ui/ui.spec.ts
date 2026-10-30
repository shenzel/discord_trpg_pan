import { expect, test, type Page } from "@playwright/test";

/** モックを開く。猶予のカウントダウンで画像が揺れないよう、still で止めて撮る */
async function open(page: Page, query: string) {
  await page.goto(`/?still&${query}`);
  await expect(page.locator("#list .row").first()).toBeVisible();
}

const screens: [string, string][] = [
  ["シナリオ前", "phase=before"],
  ["シナリオ中", "phase=during"],
  ["感想戦", "phase=after"],
  ["2垢モード", "phase=during&dual"],
  ["PLの画面", "phase=during&pl"],
  ["LとRの端を寄せた", "phase=after&span=-0.3,0.6"],
];

for (const [name, query] of screens) {
  test(name, async ({ page }) => {
    await open(page, query);
    await expect(page).toHaveScreenshot(`${name}.png`);
  });
}

test("重なった点のメニュー", async ({ page }) => {
  await open(page, "phase=after&span=0.2,0.35");
  await page.locator(".dot").first().dispatchEvent("pointerdown");
  await expect(page.locator("#popover [data-pick]")).toHaveCount(5);
  await expect(page).toHaveScreenshot("重なった点のメニュー.png");
});

test("⋯メニュー（GM）", async ({ page }) => {
  await open(page, "phase=during");
  await page.locator('[data-menu="1"]').click();
  await expect(page.locator("#popover")).toBeVisible();
  await expect(page).toHaveScreenshot("メニュー_GM.png");
});

test("⋯メニュー（PL）", async ({ page }) => {
  await open(page, "phase=during&pl");
  await page.locator('[data-menu="1"]').click();
  await expect(page.locator("#popover")).toBeVisible();
  await expect(page).toHaveScreenshot("メニュー_PL.png");
});

test("設定", async ({ page }) => {
  await open(page, "phase=during&dual");
  await page.locator("#settings-toggle").click();
  await expect(page.locator("#settings")).toBeVisible();
  await expect(page).toHaveScreenshot("設定.png");
});
