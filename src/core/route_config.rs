//! Routing preferences are separate from the credential registry and vault.
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::error::{Error, Result};
use crate::storage::fsutil;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ranking {
    Priority,
    #[default]
    Smart,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteGroup {
    /// Listed order breaks ties; an empty list includes every saved account.
    pub accounts: Vec<String>,
    /// Larger explicit priority comes first with Priority ranking.
    pub priorities: BTreeMap<String, i32>,
    pub ranking: Ranking,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingFile {
    pub schema_version: u32,
    pub groups: BTreeMap<String, RouteGroup>,
}

impl Default for RoutingFile {
    fn default() -> Self {
        Self {
            schema_version: 1,
            groups: BTreeMap::new(),
        }
    }
}

impl RoutingFile {
    pub fn load(path: &Path) -> Result<Self> {
        let Some(bytes) =
            fsutil::read_optional(path).map_err(|e| Error::io("reading routing.toml", e))?
        else {
            return Ok(Self::default());
        };
        if bytes.len() > 256 * 1024 {
            return Err(Error::usage("routing.toml exceeds 256 KiB"));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| Error::usage("routing.toml must be UTF-8"))?;
        let file: Self =
            toml::from_str(text).map_err(|e| Error::usage(format!("invalid routing.toml: {e}")))?;
        if file.schema_version != 1 {
            return Err(Error::usage("unsupported routing.toml schema_version"));
        }
        Ok(file)
    }

    pub fn group(&self, name: Option<&str>) -> Result<RouteGroup> {
        match name {
            Some(name) => self.groups.get(name).cloned().ok_or_else(|| {
                Error::usage(format!("unknown routing group `{name}`"))
                    .hint("define the group in routing.toml next to config.toml")
            }),
            None => Ok(RouteGroup::default()),
        }
    }
}
