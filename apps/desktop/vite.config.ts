import { defineConfig } from "vitest/config";
import { configDefaults } from "vitest/config";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  test: {
    // Playwright specs under e2e/ are driven by `npm run e2e`, not vitest.
    exclude: [...configDefaults.exclude, "e2e/**"],
  },
});
