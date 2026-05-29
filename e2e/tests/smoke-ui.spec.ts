import { expect, test } from "@playwright/test";

test.describe("hive-ui SSR smoke", () => {
  test("home loads with hive brand", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("a.hive-brand")).toHaveText("hive");
  });

  test("journal stylesheet loads (cream canvas, not blank white)", async ({
    page,
  }) => {
    await page.goto("/");
    const bg = await page.evaluate(
      () => getComputedStyle(document.body).backgroundColor,
    );
    // #f5efe2 → rgb(245, 239, 226)
    expect(bg).toBe("rgb(245, 239, 226)");
    await expect(page.locator("ol.feed-list .entry-body").first()).toBeVisible();
  });

  test("tasks filter is URL-driven GET form with selected owner", async ({
    page,
  }) => {
    await page.goto("/tasks?owner=pia");
    const form = page.locator('form.filters[method="get"][action="/tasks"]');
    await expect(form).toBeVisible();
    await expect(form.locator('select[name="owner"] option[value="pia"]')).toHaveAttribute(
      "selected",
      "",
    );
    await expect(form.locator('button.filter-apply[type="submit"]')).toBeVisible();
  });

  test("journal filter form submits via GET", async ({ page }) => {
    await page.goto("/journal?ai=pia");
    const form = page.locator('form.filters[method="get"][action="/journal"]');
    await expect(form).toBeVisible();
    await expect(form.locator('select[name="ai"] option[value="pia"]')).toHaveAttribute(
      "selected",
      "",
    );
  });

  test("compose page renders journal form", async ({ page }) => {
    await page.goto("/journal/new");
    await expect(page.locator('form[action="/journal/new"]')).toBeVisible();
    await expect(page.locator('textarea[name="body"]')).toBeVisible();
  });

  test("top nav exposes tasks, notes, wire", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator('a.hive-nav-link[href="/tasks"]')).toBeVisible();
    await expect(page.locator('a.hive-nav-link[href="/notes"]')).toBeVisible();
    await expect(page.locator('a.hive-nav-link[href="/wire"]')).toBeVisible();
  });
});
