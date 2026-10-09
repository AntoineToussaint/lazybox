//! Clipboard candidates from terminal text, independent of mobile presentation.
//! Fences are explicit; indentation is only a candidate, never proof of shell syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CopyItem {
    Link(String),
    Block(String),
}

/// Recognize complete shell/unlabelled fences and indented paragraphs.
/// Keep source indentation for the review; never execute or silently fix syntax.
pub(crate) fn blocks(lines: &[String]) -> Vec<(usize, CopyItem)> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let text = lines[i].trim_start();
        if let Some((marker, length, language)) = fence(text) {
            let start = i + 1;
            i = start;
            while i < lines.len() && !closes(&lines[i], marker, length) {
                i += 1;
            }
            if i < lines.len()
                && i > start
                && matches!(language, "" | "bash" | "sh" | "shell" | "zsh" | "fish")
            {
                result.push((i, CopyItem::Block(lines[start..i].join("\n"))));
            }
            i += 1;
        } else if indented(&lines[i]) {
            let start = i;
            i += 1;
            while i < lines.len() && (lines[i].trim().is_empty() || indented(&lines[i])) {
                i += 1;
            }
            let mut end = i;
            while end > start && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            result.push((end - 1, CopyItem::Block(lines[start..end].join("\n"))));
        } else {
            i += 1;
        }
    }
    result
}
fn indented(line: &str) -> bool {
    !line.trim().is_empty() && (line.starts_with("  ") || line.starts_with('\t'))
}
fn fence(text: &str) -> Option<(char, usize, &str)> {
    let marker = text.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let length = text.chars().take_while(|c| *c == marker).count();
    (length >= 3).then(|| (marker, length, text[length..].trim()))
}
fn closes(line: &str, marker: char, length: usize) -> bool {
    fence(line.trim_start())
        .is_some_and(|(m, n, rest)| m == marker && n >= length && rest.is_empty())
}

/// Position is the logical source line, not the length of a wrapped preview.
/// Deduplicate after ordering so repeated recommendations retain their newest place.
pub(crate) fn newest_first(mut items: Vec<(usize, CopyItem)>) -> Vec<CopyItem> {
    items.sort_by_key(|(line, item)| {
        (
            std::cmp::Reverse(*line),
            std::cmp::Reverse(matches!(item, CopyItem::Block(_))),
        )
    });
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .map(|(_, item)| item)
        .filter(|item| {
            let (kind, text) = match item {
                CopyItem::Link(s) => (false, s),
                CopyItem::Block(s) => (true, s),
            };
            seen.insert((kind, text.clone()))
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_blocks_preserve_blank_lines_and_ignore_incomplete_or_other_fences() {
        let lines = "explanation\n```bash\ncat <<'EOF'\n  日本\n\nEOF\n```\nafter\n  printf 'a'\n\n  echo b\nafter\n```python\n  print(1)\n```\n```sh\nincomplete".lines().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            blocks(&lines),
            vec![
                (6, CopyItem::Block("cat <<'EOF'\n  日本\n\nEOF".into())),
                (10, CopyItem::Block("  printf 'a'\n\n  echo b".into()))
            ]
        );
    }
    #[test]
    fn newest_link_and_script_share_one_order_and_repeats_move_to_top() {
        let link = CopyItem::Link("https://example.com".into());
        let script = CopyItem::Block("echo hello".into());
        assert_eq!(
            newest_first(vec![
                (1, link.clone()),
                (9, script.clone()),
                (12, link.clone())
            ]),
            vec![link.clone(), script.clone()]
        );
        assert_eq!(
            newest_first(vec![(20, script.clone()), (12, link.clone())]),
            vec![script, link]
        );
    }
}
