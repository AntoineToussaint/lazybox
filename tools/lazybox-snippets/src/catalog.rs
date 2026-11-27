use crate::Entry;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnippetAction {
    DeepReview,
    FixAll,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snippet {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_next",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub next: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<SnippetAction>,
    #[serde(skip)]
    pub origin: SnippetOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnippetOrigin {
    #[default]
    Unknown,

    Global,
    Repo,
}

impl SnippetOrigin {
    pub fn label(self) -> &'static str {
        match self {
            SnippetOrigin::Global => "global",
            SnippetOrigin::Repo => "repo",
            SnippetOrigin::Unknown => "",
        }
    }
}
impl Snippet {
    pub fn normalize_skill(&mut self) {
        self.skill = self.skill.take().and_then(|name| {
            let trimmed = name.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });
    }

    pub fn normalize_next(&mut self, key: &str) {
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

    pub fn dispatch_body(&self) -> String {
        match &self.skill {
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

    pub fn delivery_body(&self) -> String {
        self.dispatch_body()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
        (profile.join("snippets.yaml"), SnippetOrigin::Global),
        (
            directory.join(".lazybox/snippets.yaml"),
            SnippetOrigin::Repo,
        ),
    ] {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let overrides: SnippetsFile = serde_yaml::from_str(&raw)?;
        for (key, mut snippet) in overrides.snippets {
            snippet.normalize_skill();
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
                    origin: origin.label().into(),
                },
            );
        }
    }
    Ok(entries.into_values().collect())
}
