//! Read-only catalog adapter. The V1 configuration library remains authoritative
//! for profile paths, overrides, skill dispatch and delivered snippet bodies.
use lazybox_config::Snippets;
use serde::Serialize;

#[derive(Serialize)]
struct Entry {
    key: String,
    category: String,
    description: String,
    body: String,
    origin: String,
    next: Vec<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::env::current_dir()?;
    let catalog = Snippets::try_load_for_launch_dir(Some(&directory))?;
    print!("{}", serialize_catalog(&catalog)?);
    Ok(())
}

fn serialize_catalog(catalog: &Snippets) -> Result<String, serde_yaml::Error> {
    let entries: Vec<_> = catalog
        .all()
        .map(|(key, snippet)| Entry {
            key: key.to_owned(),
            category: snippet.category.clone(),
            description: snippet.description.clone(),
            body: snippet.delivery_body(),
            origin: snippet.origin.label().to_owned(),
            next: snippet.next.clone(),
        })
        .collect();
    serde_yaml::to_string(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn export_preserves_canonical_delivery() {
        let catalog = Snippets::builtin();
        let yaml = serialize_catalog(&catalog).expect("serialize catalog");
        let rows: Vec<serde_yaml::Value> = serde_yaml::from_str(&yaml).expect("read catalog");
        assert_eq!(rows.len(), catalog.len());
        assert!(!rows.is_empty());
        for row in rows {
            let key = row["key"].as_str().expect("key");
            assert_eq!(
                row["body"].as_str(),
                Some(
                    catalog
                        .get(key)
                        .expect("known key")
                        .delivery_body()
                        .as_str()
                )
            );
            assert_eq!(row["origin"].as_str(), Some("built-in"));
        }
    }
}
