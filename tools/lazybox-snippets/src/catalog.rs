use crate::Entry;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
#[derive(Deserialize)]
#[serde(untagged)]
enum ScalarOrSeq {
    One(String),
    Many(Vec<String>),
}

fn deserialize_next<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<ScalarOrSeq>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(ScalarOrSeq::One(one)) => vec![one],
        Some(ScalarOrSeq::Many(many)) => many,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SnippetAction {
    DeepReview,
    FixAll,
}

#[derive(Deserialize)]
struct Snippet {
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: String,
    body: String,
    skill: Option<String>,
    #[serde(default, deserialize_with = "deserialize_next")]
    next: Vec<String>,
    // Validate authored workflow actions even though the picker only exports text.
    #[serde(rename = "action")]
    _action: Option<SnippetAction>,
}

impl Snippet {
    fn normalize_next(&mut self, key: &str) {
        let mut seen: Vec<String> = Vec::with_capacity(self.next.len());
        for target in std::mem::take(&mut self.next) {
            let trimmed = target.trim();
            if trimmed.is_empty() || trimmed == key || seen.iter().any(|s| s == trimmed) {
                continue;
            }
            seen.push(trimmed.to_string());
        }
        self.next = seen;
    }

    fn delivery_body(&self) -> String {
        match self
            .skill
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            None => self.body.clone(),
            Some(skill) => {
                let body = self.body.trim();
                if body.is_empty() {
                    format!("Use the `{skill}` skill.")
                } else {
                    format!("Use the `{skill}` skill to complete this task:\n\n{body}")
                }
            }
        }
    }
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct SnippetsFile {
    snippets: BTreeMap<String, Snippet>,
}

pub fn load(profile: &Path, directory: &Path) -> Result<Vec<Entry>, Box<dyn std::error::Error>> {
    let builtins: Vec<Entry> = serde_yaml::from_str(include_str!("../builtin.yaml"))?;
    let mut entries: BTreeMap<_, _> = builtins
        .into_iter()
        .map(|entry| (entry.key.clone(), entry))
        .collect();
    for (path, origin) in [
        (profile.join("snippets.yaml"), "global"),
        (directory.join(".lazybox/snippets.yaml"), "repo"),
    ] {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let overrides: SnippetsFile = serde_yaml::from_str(&raw)?;
        for (key, mut snippet) in overrides.snippets {
            snippet.normalize_next(&key);
            let body = snippet.delivery_body();
            entries.insert(
                key.clone(),
                Entry {
                    key,
                    body,
                    category: snippet.category,
                    description: snippet.description,
                    next: snippet.next,
                    origin: origin.into(),
                },
            );
        }
    }
    Ok(entries.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn bundled_entries_have_bodies_and_origins() {
        let root = tempdir().unwrap();
        let shipped: Vec<Entry> = serde_yaml::from_str(include_str!("../builtin.yaml")).unwrap();
        let loaded = load(root.path(), root.path()).unwrap();
        assert!(!shipped.is_empty());
        assert_eq!(shipped.len(), loaded.len());
        for (expected, actual) in shipped.iter().zip(loaded) {
            assert_eq!(actual.key, expected.key);
            assert_eq!(actual.body, expected.body);
            assert!(!actual.body.trim().is_empty());
            assert_eq!(actual.origin, "built-in");
        }
    }

    #[test]
    fn workspace_overrides_profile_and_profile_overrides_builtins() {
        let root = tempdir().unwrap();
        let profile = root.path();
        let repo = root.path().join("workspace");
        fs::create_dir_all(repo.join(".lazybox")).unwrap();
        fs::write(
            profile.join("snippets.yaml"),
            "snippets:\n  adr: {body: global}\n",
        )
        .unwrap();
        let adr = || {
            load(profile, &repo)
                .unwrap()
                .into_iter()
                .find(|e| e.key == "adr")
                .unwrap()
        };
        assert_eq!(
            (adr().origin, adr().body),
            ("global".into(), "global".into())
        );
        fs::write(
            repo.join(".lazybox/snippets.yaml"),
            "snippets:\n  adr: {body: ' do it ', skill: ' audit ', next: [' pr ', adr, '', pr]}\n",
        )
        .unwrap();
        let entry = adr();
        assert_eq!(entry.origin, "repo");
        assert_eq!(
            entry.body,
            "Use the `audit` skill to complete this task:\n\ndo it"
        );
        assert_eq!(entry.next, ["pr"]);
    }

    #[test]
    fn invalid_layers_surface_errors() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("snippets.yaml");
        for yaml in ["snippets: [", "snippets: {a: {body: a, action: typo}}"] {
            fs::write(&file, yaml).unwrap();
            assert!(load(dir.path(), dir.path()).is_err());
        }
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        assert!(load(dir.path(), dir.path()).is_err());
    }
}
