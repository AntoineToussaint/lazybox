import { describe, expect, it } from "vitest";
import html from "../index.html?raw";

describe("desktop accessibility contract", () => {
  it("names the primary regions and live status surfaces", () => {
    expect(html).toContain('<main class="workspace-grid" aria-label=');
    expect(html).toContain('role="listbox" aria-label="Workspaces"');
    expect(html).toContain('role="status" aria-live="polite"');
    expect(html).toContain('role="alert"');
  });

  it("gives every modal a heading and an explicit cancel path", () => {
    expect(html.match(/<dialog /g)).toHaveLength(4);
    expect(html.match(/aria-labelledby=/g)?.length).toBeGreaterThanOrEqual(7);
    expect(html).toContain('id="setup-cancel-button"');
    expect(html).toContain('id="reply-cancel-button"');
    expect(html).toContain('id="new-workspace-cancel-button"');
    expect(html).toContain('id="close-terminal-cancel-button"');
  });

  it("keeps all desktop inputs programmatically labelled", () => {
    for (const id of [
      "search-input",
      "filter-select",
      "agent-select",
      "organization-select",
      "setup-agent-select",
      "analytics-checkbox",
      "crash-checkbox",
      "reply-body",
      "new-workspace-project",
      "new-workspace-name",
    ]) {
      expect(html).toMatch(
        new RegExp(`<label[^>]*>[\\s\\S]*?id="${id}"[\\s\\S]*?</label>`),
      );
    }
  });
});
