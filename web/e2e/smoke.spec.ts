import { expect, test } from "@playwright/test";
import { Daemon, startDaemon } from "./daemon";

let d: Daemon;
let port = 18790;
// A fresh daemon per test keeps sessions, plans and pairing state independent.
test.beforeEach(async () => { d = await startDaemon(port++); });
test.afterEach(() => d?.stop());

for (const vp of [
  { name: "mobile", width: 390, height: 844 },
  { name: "desktop", width: 1280, height: 800 },
]) {
  test.describe(vp.name, () => {
    test.use({ viewport: { width: vp.width, height: vp.height } });

    test("pair, chat, approve a permission and run a delegation plan", async ({ page }) => {
      // The pairing link carries the code in the fragment; the app strips it.
      const link = d.pairLink();
      expect(link).toContain("/#pair=");
      await page.goto(link);
      await expect(page.getByText("Alpha")).toBeVisible();
      expect(page.url()).not.toContain("pair=");

      // Chat: the mock agent echoes.
      await page.getByRole("button", { name: /Alpha/ }).click();
      await page.getByLabel("Message").fill("hello web");
      await page.getByRole("button", { name: "Send" }).click();
      await expect(page.getByText(/^mock reply:/).first()).toBeVisible();
      await expect(page.getByText("mock edit of src/lib.rs")).toBeVisible();

      // Permission: surfaces in the inbox with a badge and can be approved.
      await page.getByLabel("Message").fill("perm");
      await page.getByRole("button", { name: "Send" }).click();
      await expect(page.getByRole("group", { name: "Permission request" })).toBeVisible();
      await page.getByRole("button", { name: "Inbox" }).click();
      const card = page.getByRole("group", { name: "Permission request" });
      await expect(card).toBeVisible();
      await card.getByRole("button", { name: /allow/i }).first().click();
      await expect(card).toHaveCount(0);
      await page.getByRole("button", { name: "Chat", exact: true }).click();
      await expect(page.getByText(/permission outcome: selected/)).toBeVisible();

      // Delegation: propose -> review -> approve -> done.
      await page.getByRole("button", { name: "Plans" }).click();
      await page.getByLabel("Goal").fill("ship it");
      await page.getByRole("button", { name: "Propose plan" }).click();
      const plans = page.locator(".pane.plans");
      await expect(plans.getByText("do the first part", { exact: true })).toBeVisible({ timeout: 20_000 });
      await page.getByRole("button", { name: "Approve" }).click();
      try {
        await expect(plans.getByText("Done", { exact: true })).toBeVisible({ timeout: 30_000 });
      } catch (e) {
        const r = await d.rpc("plan/list", { workspace_id: d.ids.workspace });
        console.log(JSON.stringify(r.plans.map((p: any) => ({ s: p.state, e: p.error, t: p.tasks.map((t: any) => [t.state, t.error]) }))));
        console.log(await plans.innerText());
        throw e;
      }
    });

    test("an unpaired browser cannot pair with a wrong code", async ({ page }) => {
      await page.goto(d.url);
      await expect(page.getByLabel("Pairing code")).toBeVisible();
      await page.getByLabel("Pairing code").fill("WRONGCODE1");
      await page.getByRole("button", { name: "Pair this device" }).click();
      await expect(page.getByRole("alert")).toContainText("invalid or expired");
    });
  });
}

test("a revoked device is sent back to the pairing screen", async ({ page }) => {
  await page.goto(d.pairLink());
  await expect(page.getByText("Alpha")).toBeVisible();
  const { devices } = await d.rpc("web/device_list");
  await d.rpc("web/device_revoke", { device_id: devices[0].id });
  await page.reload();
  await expect(page.getByLabel("Pairing code")).toBeVisible();
});
