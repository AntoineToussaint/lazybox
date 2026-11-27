mod catalog;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Deserialize, Serialize)]
struct Entry {
    key: String,
    category: String,
    description: String,
    body: String,
    origin: String,
    next: Vec<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let profile = std::env::var("LAZYBOX_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".lazybox")
        });
    let entries = catalog::load(&profile, &std::env::current_dir()?)?;
    print!("{}", serde_yaml::to_string(&entries)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn locked_build_has_no_git_dependencies() {
        assert!(!include_str!("../Cargo.lock").contains("source = \"git+"));
    }
}
