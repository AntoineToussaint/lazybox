// @vitest-environment happy-dom

import { describe, expect, it } from "vitest";
import type { DiffFileDto, WorkspaceDiffDto } from "./protocol";
import { MAX_DIFF_LINES, buildDiffView, fileHeading } from "./diff_view";

function file(overrides: Partial<DiffFileDto> & { path: string }): DiffFileDto {
  return {
    old_path: overrides.old_path ?? null,
    path: overrides.path,
    headers: overrides.headers ?? [],
    hunks: overrides.hunks ?? [],
  };
}

function diff(overrides: Partial<WorkspaceDiffDto> = {}): WorkspaceDiffDto {
  return {
    status: overrides.status ?? [],
    stat: overrides.stat ?? [],
    files: overrides.files ?? [],
    truncated: overrides.truncated ?? false,
  };
}

describe("fileHeading", () => {
  it("shows a rename as old → new", () => {
    expect(fileHeading(file({ old_path: "a.rs", path: "b.rs" }))).toBe(
      "a.rs → b.rs",
    );
  });

  it("shows just the path when it did not move", () => {
    expect(fileHeading(file({ old_path: "a.rs", path: "a.rs" }))).toBe("a.rs");
    expect(fileHeading(file({ path: "a.rs" }))).toBe("a.rs");
  });
});

describe("buildDiffView", () => {
  it("renders a clean note when nothing changed", () => {
    const view = buildDiffView(diff());
    expect(view.querySelector(".diff-clean")?.textContent).toContain("clean");
    expect(view.querySelector(".diff-file")).toBeNull();
  });

  it("renders the diffstat, files, hunks, and coloured lines", () => {
    const view = buildDiffView(
      diff({
        stat: [" src/main.rs | 2 +-"],
        files: [
          file({
            path: "src/main.rs",
            hunks: [
              {
                header: "@@ -1,2 +1,2 @@",
                old_start: 1,
                new_start: 1,
                lines: [
                  {
                    kind: "Context",
                    text: " let x = 1;",
                    old_line: 1,
                    new_line: 1,
                  },
                  {
                    kind: "Deletion",
                    text: "-let y = 2;",
                    old_line: 2,
                    new_line: null,
                  },
                  {
                    kind: "Addition",
                    text: "+let y = 3;",
                    old_line: null,
                    new_line: 2,
                  },
                ],
              },
            ],
          }),
        ],
      }),
    );

    expect(view.querySelector(".diff-stat")?.textContent).toContain(
      "src/main.rs",
    );
    expect(view.querySelector(".diff-file-path")?.textContent).toBe(
      "src/main.rs",
    );
    // Hunk header row + three diff lines = four rows.
    expect(view.querySelectorAll(".diff-line")).toHaveLength(4);
    expect(view.querySelector(".diff-line.hunk")?.textContent).toContain(
      "@@ -1,2 +1,2 @@",
    );
    expect(view.querySelector(".diff-line.addition")?.textContent).toContain(
      "+let y = 3;",
    );
    expect(view.querySelector(".diff-line.deletion")?.textContent).toContain(
      "-let y = 2;",
    );
  });

  it("shows a gutter number only for the side a line touches", () => {
    const view = buildDiffView(
      diff({
        files: [
          file({
            path: "a.rs",
            hunks: [
              {
                header: "@@ -1 +1 @@",
                old_start: 1,
                new_start: 1,
                lines: [
                  {
                    kind: "Addition",
                    text: "+added",
                    old_line: null,
                    new_line: 5,
                  },
                ],
              },
            ],
          }),
        ],
      }),
    );
    const addition = view.querySelector(".diff-line.addition");
    const gutters = addition?.querySelectorAll(".diff-gutter");
    expect(gutters?.[0]?.textContent).toBe("");
    expect(gutters?.[1]?.textContent).toBe("5");
  });

  it("appends a truncation notice", () => {
    const view = buildDiffView(
      diff({ files: [file({ path: "a.rs" })], truncated: true }),
    );
    expect(view.querySelector(".diff-truncated")).not.toBeNull();
  });

  it("caps how many lines it renders and flags the overflow", () => {
    const overflow = MAX_DIFF_LINES + 500;
    const lines = Array.from({ length: overflow }, (_, i) => ({
      kind: "Addition" as const,
      text: `+line ${i}`,
      old_line: null,
      new_line: i + 1,
    }));
    const view = buildDiffView(
      diff({
        files: [
          file({
            path: "big.rs",
            hunks: [
              {
                header: `@@ -1 +1,${overflow} @@`,
                old_start: 1,
                new_start: 1,
                lines,
              },
            ],
          }),
        ],
      }),
    );

    // Never build more DOM rows than the budget, however big the diff.
    expect(view.querySelectorAll(".diff-line").length).toBeLessThanOrEqual(
      MAX_DIFF_LINES,
    );
    const notice = view.querySelector(".diff-truncated");
    expect(notice?.textContent).toContain("too large");
  });

  it("renders a diff of exactly the budget in full, unflagged", () => {
    // One hunk header + (MAX - 1) lines = exactly MAX_DIFF_LINES rows.
    const lines = Array.from({ length: MAX_DIFF_LINES - 1 }, (_, i) => ({
      kind: "Addition" as const,
      text: `+line ${i}`,
      old_line: null,
      new_line: i + 1,
    }));
    const view = buildDiffView(
      diff({
        files: [
          file({
            path: "exact.rs",
            hunks: [
              { header: "@@ -1 +1 @@", old_start: 1, new_start: 1, lines },
            ],
          }),
        ],
      }),
    );
    expect(view.querySelectorAll(".diff-line").length).toBe(MAX_DIFF_LINES);
    expect(view.querySelector(".diff-truncated")).toBeNull();
  });

  it("does not flag a diff that fits within the budget", () => {
    const view = buildDiffView(
      diff({
        files: [
          file({
            path: "small.rs",
            hunks: [
              {
                header: "@@ -1 +1 @@",
                old_start: 1,
                new_start: 1,
                lines: [
                  {
                    kind: "Addition",
                    text: "+one line",
                    old_line: null,
                    new_line: 1,
                  },
                ],
              },
            ],
          }),
        ],
      }),
    );
    expect(view.querySelector(".diff-truncated")).toBeNull();
  });
});
