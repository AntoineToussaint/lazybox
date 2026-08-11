import { expect, test, type Page } from "@playwright/test";

// Representative chrome regions with solid, theme-derived backgrounds (topbar,
// sidebar, activity panel). Reading their computed luminance catches a
// hardcoded-dark/light "island" that a single body-background check would
// miss. The terminal panel is intentionally excluded: its background is
// owned by the embedded xterm and is transparent until a tile mounts.
const CHROME_REGIONS = [".topbar", ".inbox-panel", ".activity-panel"];

async function chromeRegionLuminances(page: Page): Promise<number[]> {
  return page.evaluate((selectors) => {
    const relativeLuminance = (color: string): number => {
      const channels = (color.match(/[\d.]+/g) ?? []).slice(0, 3).map(Number);
      // Chromium reports color-mix() results as `color(srgb r g b / a)` with
      // 0–1 floats, but plain colors as `rgb(r, g, b)` with 0–255 ints — the
      // theme's chrome mixes hit the former, terminal panels the latter.
      const isSrgbFloat = color.includes("srgb");
      const [r, g, b] = channels.map((channel) => {
        const c = isSrgbFloat ? channel : channel / 255;
        return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
      });
      return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    };
    return selectors.map((selector) => {
      const element = document.querySelector(selector);
      if (element === null) {
        throw new Error(`missing chrome region: ${selector}`);
      }
      const background = getComputedStyle(element).backgroundColor;
      // A transparent region shows its parent, not an island — guard against
      // silently scoring it as pure black (rgba(0,0,0,0)).
      if (/,\s*0\s*\)$/.test(background) || background === "transparent") {
        throw new Error(`chrome region ${selector} has no opaque background`);
      }
      return relativeLuminance(background);
    });
  }, CHROME_REGIONS);
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    class NotificationStub {
      static permission: NotificationPermission = "default";
      static async requestPermission(): Promise<NotificationPermission> {
        NotificationStub.permission = "granted";
        return "granted";
      }
      onclick: (() => void) | null = null;
      constructor(
        public readonly title: string,
        public readonly options?: NotificationOptions,
      ) {}
    }
    Object.defineProperty(window, "Notification", {
      configurable: true,
      value: NotificationStub,
    });
  });
  await page.goto("/?preview");
  await expect(page.getByText("Preview data")).toBeVisible();
});

test("boots with closed dialogs and preserves keyboard pane state across reload", async ({
  page,
}) => {
  await expect(page.locator("dialog[open]")).toHaveCount(0);
  await page.keyboard.press("ArrowDown");
  await expect(
    page.locator('.workspace-row[aria-current="true"]'),
  ).toBeFocused();

  const splitter = page.getByRole("separator", { name: "Resize inbox" });
  await splitter.focus();
  await page.keyboard.press("ArrowRight");
  await expect(splitter).toHaveAttribute("aria-valuenow", "376");
  await page.reload();
  await expect(
    page.getByRole("separator", { name: "Resize inbox" }),
  ).toHaveAttribute("aria-valuenow", "376");
  await expect(page.getByText("Preview data")).toBeVisible();
});

test("applies complete themes and exercises notification permission", async ({
  page,
}) => {
  await page.getByRole("button", { name: "Open settings" }).click();
  await expect(
    page.getByRole("dialog", { name: "Desktop settings" }),
  ).toBeVisible();
  await expect(page.getByText("Notification permission: prompt")).toBeVisible();
  await page.getByRole("button", { name: "Enable notifications" }).click();
  await expect(
    page.getByText("Notification permission: granted"),
  ).toBeVisible();

  // Each theme must repaint the WHOLE chrome, not just accents. Assert both
  // the app background (the theme's `surface`) and that every representative
  // chrome region follows it — a topbar/sidebar/panel left dark in light mode
  // is a hardcoded "dark island" a body-only check can't catch. Deterministic
  // across machines, unlike a full-page pixel screenshot.
  await page.getByRole("radio", { name: "Lazybox Light" }).click();
  await expect(page.locator("body")).toHaveCSS(
    "background-color",
    "rgb(247, 248, 250)",
  );
  // Light theme: every chrome region is light (no dark island). Measured
  // ~0.72–0.92; the ≥0.4 floor cleanly excludes any dark (≤~0.05) region.
  for (const luminance of await chromeRegionLuminances(page)) {
    expect(luminance).toBeGreaterThan(0.4);
  }
  await page.getByRole("button", { name: "Close settings" }).click();

  await page.getByRole("button", { name: "Open settings" }).click();
  await page.getByRole("radio", { name: "High Contrast" }).click();
  await expect(page.locator("body")).toHaveCSS(
    "background-color",
    "rgb(0, 0, 0)",
  );
  // High contrast: every chrome region is near-black.
  for (const luminance of await chromeRegionLuminances(page)) {
    expect(luminance).toBeLessThan(0.2);
  }
  await page.getByRole("button", { name: "Close settings" }).click();
});

test("representative dark screen passes automated accessibility checks", async ({
  page,
}) => {
  await page.addScriptTag({ path: "node_modules/axe-core/axe.min.js" });
  const violations = await page.evaluate(async () => {
    const axe = (
      window as typeof window & {
        axe: {
          run: () => Promise<{
            violations: Array<{ id: string; impact: string | null }>;
          }>;
        };
      }
    ).axe;
    const result = await axe.run();
    return result.violations.filter((violation) =>
      ["critical", "serious"].includes(violation.impact ?? ""),
    );
  });
  expect(violations).toEqual([]);
  // The default dark theme paints the app background from its `surface`, and
  // every chrome region follows it (no light island in dark mode).
  await expect(page.locator("body")).toHaveCSS(
    "background-color",
    "rgb(26, 29, 46)",
  );
  for (const luminance of await chromeRegionLuminances(page)) {
    expect(luminance).toBeLessThan(0.2);
  }
});
