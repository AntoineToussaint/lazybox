import type {
  DiffFileDto,
  DiffHunkDto,
  DiffLineDto,
  WorkspaceDiffDto,
} from "./protocol";

/**
 * Cap on how many diff lines (hunk headers + content) the reader
 * renders as DOM. The daemon caps the patch at 4 MiB (≈100k lines,
 * `git-ops` `DIFF_PATCH_BYTES`), and building ~4 nodes per line eagerly
 * would freeze the webview on a regenerated lockfile or a large
 * generated file. Beyond this the reader stops and points at the
 * worktree — the same escape hatch a daemon-truncated diff uses.
 */
export const MAX_DIFF_LINES = 3000;

/** The heading label for a file section, showing renames as `old → new`. */
export function fileHeading(file: DiffFileDto): string {
  if (file.old_path !== null && file.old_path !== file.path) {
    return `${file.old_path} → ${file.path}`;
  }
  return file.path;
}

/**
 * Build the read-only diff reader body for the desktop's diff modal
 * (#843), mirroring the TUI's `ViewDiff`: a diffstat summary, then each
 * file's hunks with an old/new line-number gutter and add/delete/context
 * colouring. A worktree with no local changes renders a single "clean"
 * note; a diff that overflows `MAX_DIFF_LINES` or that the daemon already
 * truncated appends a notice so the reader never looks complete when it
 * isn't.
 */
export function buildDiffView(diff: WorkspaceDiffDto): HTMLElement {
  const root = document.createElement("div");
  root.className = "diff-view";

  if (diff.files.length === 0) {
    const clean = document.createElement("p");
    clean.className = "diff-clean";
    clean.textContent = "No local changes — the worktree is clean.";
    root.append(clean);
    return root;
  }

  if (diff.stat.length > 0) {
    const stat = document.createElement("pre");
    stat.className = "diff-stat";
    stat.textContent = diff.stat.join("\n");
    root.append(stat);
  }

  const budget = { remaining: MAX_DIFF_LINES };
  for (const file of diff.files) {
    if (budget.remaining <= 0) {
      break;
    }
    root.append(fileSection(file, budget));
  }

  // Flag from the true line count, not the leftover budget: a diff of
  // exactly MAX_DIFF_LINES renders in full and must not claim otherwise.
  const capped = totalDiffLines(diff) > MAX_DIFF_LINES;
  if (capped) {
    const note = document.createElement("p");
    note.className = "diff-truncated";
    note.textContent = `Diff too large to render (over ${MAX_DIFF_LINES} lines) — open the worktree to see the rest.`;
    root.append(note);
  } else if (diff.truncated) {
    const truncated = document.createElement("p");
    truncated.className = "diff-truncated";
    truncated.textContent = "Diff truncated — open the worktree to see the rest.";
    root.append(truncated);
  }

  return root;
}

/** Total renderable rows: one per hunk header plus every hunk line. */
function totalDiffLines(diff: WorkspaceDiffDto): number {
  let total = 0;
  for (const file of diff.files) {
    for (const hunk of file.hunks) {
      total += 1 + hunk.lines.length;
    }
  }
  return total;
}

function fileSection(file: DiffFileDto, budget: { remaining: number }): HTMLElement {
  const section = document.createElement("section");
  section.className = "diff-file";

  const heading = document.createElement("h3");
  heading.className = "diff-file-path";
  heading.textContent = fileHeading(file);
  section.append(heading);

  for (const hunk of file.hunks) {
    if (budget.remaining <= 0) {
      break;
    }
    section.append(hunkBlock(hunk, budget));
  }
  return section;
}

function hunkBlock(hunk: DiffHunkDto, budget: { remaining: number }): HTMLElement {
  const block = document.createElement("div");
  block.className = "diff-hunk";

  block.append(diffLineRow(null, null, hunk.header, "hunk"));
  budget.remaining -= 1;
  for (const line of hunk.lines) {
    if (budget.remaining <= 0) {
      break;
    }
    block.append(
      diffLineRow(line.old_line, line.new_line, line.text, lineClass(line)),
    );
    budget.remaining -= 1;
  }
  return block;
}

function lineClass(line: DiffLineDto): string {
  switch (line.kind) {
    case "Addition":
      return "addition";
    case "Deletion":
      return "deletion";
    case "Meta":
      return "meta";
    case "Context":
      return "context";
  }
}

function diffLineRow(
  oldLine: number | null,
  newLine: number | null,
  text: string,
  kind: string,
): HTMLElement {
  const row = document.createElement("div");
  row.className = `diff-line ${kind}`;

  const oldGutter = document.createElement("span");
  oldGutter.className = "diff-gutter";
  oldGutter.textContent = oldLine === null ? "" : String(oldLine);

  const newGutter = document.createElement("span");
  newGutter.className = "diff-gutter";
  newGutter.textContent = newLine === null ? "" : String(newLine);

  const content = document.createElement("span");
  content.className = "diff-content";
  content.textContent = text;

  row.append(oldGutter, newGutter, content);
  return row;
}
