//! Guards that the documented standing rules ARE the shipped standing rules.
//!
//! `web/src/content/docs/docs/reference/configuration.md` carries one table
//! row per built-in [`AgentPolicy`], and that table's whole job is to be the
//! rule's text: it is what a user reads before overriding one, and what an
//! agent reads when it is pointed at the reference. Two rows drifted into
//! paraphrase the moment they were written — one documented a
//! default-branch check the shipped rule never mentions, the other promised
//! "generated references" and "name the docs checked" that appear in neither
//! the rule nor the code. `AGENTS.md` is explicit about the cost: "A human
//! survives a stale doc; an agent reading it on every request is poisoned by
//! one."
//!
//! Lives in `lazybox-core` alongside `dep_rules.rs` and `gitattributes.rs`:
//! core is a leaf with no internal deps, the natural home for repo-wide
//! hygiene guards.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

/// Collapse the whitespace a `\`-continued Rust string literal and a
/// markdown table cell wrap differently, so the comparison is about words.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_builtin_standing_rule_is_documented_verbatim() {
    let reference = workspace_root().join("web/src/content/docs/docs/reference/configuration.md");
    let doc = normalized(&std::fs::read_to_string(&reference).expect("read the config reference"));

    let policies = lazybox_core::agent_policy::AgentPolicies::builtin();
    for id in lazybox_core::agent_policy::AgentPolicies::builtin_ids() {
        let text = policies.text(&id).expect("every builtin id resolves");
        assert!(
            doc.contains(&format!("`{id}`")),
            "`{id}` ships by default but the config reference never names it: {}",
            reference.display()
        );
        assert!(
            doc.contains(&normalized(text)),
            "the config reference paraphrases `{id}` instead of quoting the rule agents \
             receive.\n  shipped: {}\n  fix {} to carry that text verbatim.",
            normalized(text),
            reference.display(),
        );
    }
}
