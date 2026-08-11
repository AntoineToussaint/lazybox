import { expect, test } from "@playwright/test";

// #970 acceptance: the diff dialog must be absent from layout before
// showModal() and after close(). jsdom can't model the UA closed-dialog CSS
// interaction, so this needs a real browser.
test("the diff dialog leaves layout when closed", async ({ page }) => {
  await page.goto("/?preview");
  const dialog = page.locator("#diff-dialog");

  await expect(dialog).toHaveCSS("display", "none");
  await dialog.evaluate((element: HTMLDialogElement) => element.showModal());
  await expect(dialog).toHaveCSS("display", "flex");
  await dialog.evaluate((element: HTMLDialogElement) => element.close());
  await expect(dialog).toHaveCSS("display", "none");
});
