use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Personal UI data, independent of the temporary web origin and bearer.
/// Unknown/corrupt data is retained and reported, never silently overwritten.
pub struct Repository {
    path: PathBuf,
    scope: String,
    pub values: BTreeMap<String, String>,
}
impl Repository {
    pub fn open(root: &Path, scope: &str) -> Result<Self, String> {
        let path = root.join(format!("views-{scope}.json"));
        let values = match fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() > 4 << 20 {
                    return Err("Saved views exceed storage limit".into());
                }
                serde_json::from_slice(&bytes).map_err(|e| {
                    format!("Cannot restore saved views; original file retained: {e}")
                })?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.to_string()),
        };
        Ok(Self {
            path,
            scope: scope.into(),
            values,
        })
    }
    pub fn save(&mut self, key: &str, value: &str) -> Result<(), String> {
        if !key.starts_with(&format!("{}:", self.scope))
            || key.len() > 1024
            || value.len() > 256 * 1024
        {
            return Err("Invalid saved view scope or size".into());
        }
        let mut next = self.values.clone();
        next.insert(key.into(), value.into());
        let data = serde_json::to_vec(&next).map_err(|e| e.to_string())?;
        if next.len() > 4096 || data.len() > 4 << 20 {
            return Err("Saved view storage is full".into());
        }
        let parent = self.path.parent().ok_or("Saved views have no parent")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        file.write_all(&data).map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(&self.path).map_err(|e| e.to_string())?;
        self.values = next;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drafts_and_pages_survive_restart_and_remain_scoped_without_destroying_corruption() {
        let root = tempfile::tempdir().expect("fixture");
        let mut repo = Repository::open(root.path(), "runner-one").expect("open");
        repo.save("runner-one:main:draft:session", "draft text")
            .expect("save");
        assert!(repo.save("runner-two:main", "wrong").is_err());
        assert!(
            repo.save("runner-one:huge", &"x".repeat(256 * 1024 + 1))
                .is_err()
        );
        let restored = Repository::open(root.path(), "runner-one").expect("restore");
        assert_eq!(
            restored.values["runner-one:main:draft:session"],
            "draft text"
        );
        assert!(
            Repository::open(root.path(), "runner-two")
                .expect("isolated")
                .values
                .is_empty()
        );
        fs::write(&restored.path, "invalid").expect("corrupt");
        assert!(Repository::open(root.path(), "runner-one").is_err());
        assert_eq!(
            fs::read_to_string(&restored.path).expect("retained"),
            "invalid"
        );
    }
}
