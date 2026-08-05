import type {
  DiffFileDto,
  DiffHunkDto,
  DiffLineDto,
  WorkspaceDiffDto,
} from "./protocol";

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
 * note; a daemon-truncated diff appends a notice so the reader never
 * looks complete when it isn't.
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

  for (const file of diff.files) {
    root.append(fileSection(file));
  }

  if (diff.truncated) {
    const truncated = document.createElement("p");
    truncated.className = "diff-truncated";
    truncated.textContent = "Diff truncated — open the worktree to see the rest.";
    root.append(truncated);
  }

  return root;
}

function fileSection(file: DiffFileDto): HTMLElement {
  const section = document.createElement("section");
  section.className = "diff-file";

  const heading = document.createElement("h3");
  heading.className = "diff-file-path";
  heading.textContent = fileHeading(file);
  section.append(heading);

  for (const hunk of file.hunks) {
    section.append(hunkBlock(hunk));
  }
  return section;
}

function hunkBlock(hunk: DiffHunkDto): HTMLElement {
  const block = document.createElement("div");
  block.className = "diff-hunk";

  block.append(diffLineRow(null, null, hunk.header, "hunk"));
  for (const line of hunk.lines) {
    block.append(
      diffLineRow(line.old_line, line.new_line, line.text, lineClass(line)),
    );
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
