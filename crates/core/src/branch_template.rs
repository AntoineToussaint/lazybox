//! Render a Linear branch name from a house-style template.
//!
//! The obin convention is `{handle}/{type}/{id}-{slug}` — a personal
//! handle, a commit type, the ticket id, and a title slug (e.g.
//! `antoine/feat/obi-1749-template-sa-seam`). A user's
//! `providers.linear.branch_template` supplies the pattern; this module
//! substitutes the tokens and, crucially, collapses the separators an
//! empty token would otherwise orphan — an unmapped `{type}`, a blank
//! `{handle}`, or an emoji-only `{slug}` must not yield `antoine//1749`
//! or a leading `/`, both of which are invalid git refs.

use std::collections::BTreeMap;

/// Substitute `{token}` placeholders and collapse orphaned separators.
///
/// Each entry in `tokens` is `(name, value)`; a value is substituted
/// verbatim (callers pass already-sanitized `[a-z0-9-]` values), and a
/// `{name}` with no matching token becomes empty. Then, per
/// `/`-delimited segment, runs of `-` collapse to one and leading /
/// trailing `-` are trimmed; a segment left empty drops out entirely.
/// Returns `None` when nothing survives, so the caller can fall back
/// rather than build an empty ref.
pub fn render_branch_template(template: &str, tokens: &[(&str, &str)]) -> Option<String> {
    let substituted = substitute(template, tokens);
    let collapsed: Vec<String> = substituted
        .split('/')
        .map(collapse_dashes)
        .filter(|seg| !seg.is_empty())
        .collect();
    (!collapsed.is_empty()).then(|| collapsed.join("/"))
}

fn substitute(template: &str, tokens: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                if let Some((_, val)) = tokens.iter().find(|(n, _)| *n == name) {
                    out.push_str(val);
                }
                rest = &after[close + 1..];
            }
            // An unterminated `{` is emitted literally and ends the scan.
            None => {
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse_dashes(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut prev_dash = false;
    for ch in segment.chars() {
        if ch == '-' {
            if !prev_dash {
                out.push('-');
            }
            prev_dash = true;
        } else {
            out.push(ch);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

/// The `{type}` token for a Linear ticket: the highest-precedence commit
/// type among the ticket's labels, looked up through the user's
/// `label_types` (label name → type token) map. Several matching labels
/// resolve deterministically by a fixed precedence (`fix` > `feat` >
/// `chore` > `docs`, any other mapped token alphabetically after); no
/// matching label yields `None`, and the `{type}` token then collapses
/// out of the rendered branch.
pub fn type_token_for_labels<'a>(
    label_names: impl IntoIterator<Item = &'a str>,
    label_types: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    const PRECEDENCE: [&str; 4] = ["fix", "feat", "chore", "docs"];
    let rank = |t: &str| {
        PRECEDENCE
            .iter()
            .position(|p| *p == t)
            .unwrap_or(PRECEDENCE.len())
    };
    label_names
        .into_iter()
        .filter_map(|name| label_types.get(name).map(String::as_str))
        .min_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens<'a>(
        handle: &'a str,
        ty: &'a str,
        id: &'a str,
        slug: &'a str,
    ) -> Vec<(&'a str, &'a str)> {
        vec![("handle", handle), ("type", ty), ("id", id), ("slug", slug)]
    }

    const OBIN: &str = "{handle}/{type}/{id}-{slug}";

    #[test]
    fn renders_full_obin_branch() {
        let t = tokens("antoine", "feat", "obi-1749", "template-sa-seam");
        assert_eq!(
            render_branch_template(OBIN, &t).as_deref(),
            Some("antoine/feat/obi-1749-template-sa-seam"),
        );
    }

    #[test]
    fn empty_id_collapses_its_separator() {
        let t = tokens("antoine", "feat", "", "template-sa-seam");
        assert_eq!(
            render_branch_template(OBIN, &t).as_deref(),
            Some("antoine/feat/template-sa-seam"),
        );
    }

    #[test]
    fn empty_handle_drops_leading_segment() {
        let t = tokens("", "feat", "obi-1749", "ship-it");
        assert_eq!(
            render_branch_template(OBIN, &t).as_deref(),
            Some("feat/obi-1749-ship-it"),
        );
    }

    #[test]
    fn empty_type_drops_its_segment() {
        let t = tokens("antoine", "", "obi-1749", "ship-it");
        assert_eq!(
            render_branch_template(OBIN, &t).as_deref(),
            Some("antoine/obi-1749-ship-it"),
        );
    }

    #[test]
    fn empty_slug_leaves_a_clean_id() {
        let t = tokens("antoine", "feat", "obi-1749", "");
        assert_eq!(
            render_branch_template(OBIN, &t).as_deref(),
            Some("antoine/feat/obi-1749"),
        );
    }

    #[test]
    fn all_tokens_empty_yields_none() {
        let t = tokens("", "", "", "");
        assert_eq!(render_branch_template(OBIN, &t), None);
    }

    #[test]
    fn unknown_token_is_dropped() {
        assert_eq!(
            render_branch_template("{handle}/{unknown}/{id}", &tokens("a", "", "b", "")).as_deref(),
            Some("a/b"),
        );
    }

    #[test]
    fn unterminated_brace_does_not_panic() {
        // A malformed template must not panic; the stray brace is emitted
        // verbatim (token values are sanitized, template literals aren't).
        assert_eq!(
            render_branch_template("x/{id", &[("id", "y")]).as_deref(),
            Some("x/{id"),
        );
    }

    #[test]
    fn type_token_none_when_no_label_maps() {
        let map = BTreeMap::from([("Bug".to_string(), "fix".to_string())]);
        assert_eq!(type_token_for_labels(["Feature"], &map), None);
        assert_eq!(
            type_token_for_labels(std::iter::empty::<&str>(), &map),
            None
        );
    }

    #[test]
    fn type_token_single_match() {
        let map = BTreeMap::from([("Feature".to_string(), "feat".to_string())]);
        assert_eq!(type_token_for_labels(["Feature", "P1"], &map), Some("feat"));
    }

    #[test]
    fn type_token_precedence_prefers_fix_over_feat() {
        let map = BTreeMap::from([
            ("Bug".to_string(), "fix".to_string()),
            ("Feature".to_string(), "feat".to_string()),
        ]);
        // Regardless of label ordering the precedence is deterministic.
        assert_eq!(type_token_for_labels(["Feature", "Bug"], &map), Some("fix"),);
        assert_eq!(type_token_for_labels(["Bug", "Feature"], &map), Some("fix"),);
    }
}
