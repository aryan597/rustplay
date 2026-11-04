//! Layer 05 (access): every step is checked against the caller's role.
//! The check is on the principal running the task, never on the task itself,
//! so a task file cannot grant itself extra powers.

use crate::errors::{Result, RpError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Load URLs.
    Navigate,
    /// Read text, attributes, inline screenshots, run assertions.
    Read,
    /// Click, type, press keys, scroll.
    Interact,
    /// Write files (screenshots) into the output directory.
    WriteFiles,
    /// Run arbitrary JavaScript in the page.
    Eval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// navigate + read
    Viewer,
    /// viewer + interact + write_files
    #[default]
    Operator,
    /// operator + eval
    Admin,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }

    pub fn allows(self, c: Capability) -> bool {
        use Capability::*;
        match self {
            Role::Viewer => matches!(c, Navigate | Read),
            Role::Operator => matches!(c, Navigate | Read | Interact | WriteFiles),
            Role::Admin => true,
        }
    }

    pub fn require(self, c: Capability) -> Result<()> {
        if self.allows(c) {
            Ok(())
        } else {
            Err(RpError::Forbidden {
                role: self.name().into(),
                capability: serde_json::to_value(c).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            })
        }
    }
}

/// An authenticated API caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub name: String,
    pub role: Role,
}

/// Token table for the HTTP API, loaded from a JSON file:
/// `[{"token": "...", "name": "ci-bot", "role": "viewer"}]`
#[derive(Debug, Clone, Default)]
pub struct TokenTable {
    entries: Vec<(String, Principal)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenEntry {
    token: String,
    name: String,
    role: Role,
}

impl TokenTable {
    pub fn from_json(s: &str) -> Result<Self> {
        let raw: Vec<TokenEntry> = serde_json::from_str(s).map_err(|e| RpError::config(format!("tokens file: {e}")))?;
        let mut entries = Vec::new();
        for t in raw {
            if t.token.len() < 16 {
                return Err(RpError::config(format!("token for `{}` is shorter than 16 chars", t.name)));
            }
            entries.push((t.token, Principal { name: t.name, role: t.role }));
        }
        Ok(Self { entries })
    }

    pub fn single(token: String, role: Role) -> Self {
        Self { entries: vec![(token, Principal { name: "default".into(), role })] }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Constant-time per entry, so response timing does not leak token prefixes.
    pub fn authenticate(&self, presented: &str) -> Option<Principal> {
        let mut found = None;
        for (tok, p) in &self.entries {
            if ct_eq(tok.as_bytes(), presented.as_bytes()) {
                found = Some(p.clone());
            }
        }
        found
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_matrix() {
        assert!(Role::Viewer.allows(Capability::Read));
        assert!(!Role::Viewer.allows(Capability::Interact));
        assert!(!Role::Operator.allows(Capability::Eval));
        assert!(Role::Admin.allows(Capability::Eval));
        let e = Role::Viewer.require(Capability::WriteFiles).unwrap_err();
        assert_eq!(e.code(), "FORBIDDEN");
        assert!(e.to_string().contains("write_files"));
    }

    #[test]
    fn tokens() {
        let t = TokenTable::from_json(r#"[{"token":"aaaaaaaaaaaaaaaa","name":"bot","role":"viewer"}]"#).unwrap();
        assert_eq!(t.authenticate("aaaaaaaaaaaaaaaa").unwrap().role, Role::Viewer);
        assert!(t.authenticate("aaaaaaaaaaaaaaab").is_none());
        assert!(t.authenticate("").is_none());
        assert!(TokenTable::from_json(r#"[{"token":"short","name":"x","role":"admin"}]"#).is_err());
    }
}
