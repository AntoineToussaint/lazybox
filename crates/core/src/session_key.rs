//! `SessionKey` — a cheap-to-clone identifier for a session.
//!
//! A session is uniquely identified by `TaskId`, but the string form
//! (`"github:owner/repo#123"`) is used as a map key throughout the app.
//! Using `String` means every `clone()` hits the allocator; this newtype
//! wraps `Arc<str>` so `clone()` becomes a refcount bump.
//!
//! Zero-cost for map lookups: `Borrow<str>` lets callers look up with
//! `&str`, so existing `HashMap<String, _>` users can migrate opportunistically
//! without forcing every caller to convert.

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::TaskId;

/// Unique identifier for a `Session`. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
#[serde(transparent)]
pub struct SessionKey(#[cfg_attr(feature = "desktop-contract", ts(type = "string"))] Arc<str>);

impl SessionKey {
    /// Build a key from any string-like input.
    pub fn new(s: impl Into<Arc<str>>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SessionKey {
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl From<String> for SessionKey {
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

impl From<&TaskId> for SessionKey {
    fn from(id: &TaskId) -> Self {
        Self::from(id.to_string().as_str())
    }
}

impl From<TaskId> for SessionKey {
    fn from(id: TaskId) -> Self {
        Self::from(&id)
    }
}

impl From<&SessionKey> for SessionKey {
    fn from(k: &SessionKey) -> Self {
        k.clone()
    }
}

impl From<&crate::WorkspaceKey> for SessionKey {
    fn from(k: &crate::WorkspaceKey) -> Self {
        Self::from(k.as_str())
    }
}

impl From<crate::WorkspaceKey> for SessionKey {
    fn from(k: crate::WorkspaceKey) -> Self {
        Self::from(k.as_str())
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Deref for SessionKey {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SessionKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for SessionKey {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for SessionKey {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for SessionKey {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for SessionKey {
    fn eq(&self, other: &String) -> bool {
        &*self.0 == other.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn clone_is_cheap() {
        let k = SessionKey::from("github:owner/repo#1");
        let c = k.clone();
        // Same Arc backing storage — compare the wide-pointer data addresses.
        assert!(std::ptr::eq(&*k.0 as *const str, &*c.0 as *const str));
    }

    #[test]
    fn hashmap_lookup_by_str() {
        let mut m: HashMap<SessionKey, i32> = HashMap::new();
        m.insert(SessionKey::from("k"), 42);
        // Borrow<str> makes this work without converting on the query side.
        assert_eq!(m.get("k"), Some(&42));
    }

    #[test]
    fn display_and_deref() {
        let k = SessionKey::from("k1");
        assert_eq!(format!("{k}"), "k1");
        assert_eq!(&*k, "k1");
        assert_eq!(k.len(), 2);
    }

    #[test]
    fn from_task_id() {
        let id = TaskId {
            source: "github".into(),
            key: "o/r#1".into(),
        };
        let k: SessionKey = (&id).into();
        assert_eq!(k.as_str(), "github:o/r#1");
    }

    #[test]
    fn cross_type_equality() {
        let k = SessionKey::from("x");
        assert_eq!(k, "x");
        assert_eq!(k, String::from("x"));
    }
}
