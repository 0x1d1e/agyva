//! agy's per-account companion state next to the credential:
//! `installation_id` (plain text UUID) and the `gcp` block inside
//! `settings.json`. Only that block of `settings.json` is ever modified.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::error::{Error, Result};
use crate::paths::AgyPaths;
use crate::storage::fsutil;

/// Companion state of one account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Companion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    /// Contents of the `gcp` key of `settings.json`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gcp: Option<Value>,
}

/// Normalize a UUID string (lowercase, trimmed); `None` if it is not one.
pub fn normalize_installation_id(value: &str) -> Option<String> {
    let id = value.trim().to_ascii_lowercase();
    let ok = id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    ok.then_some(id)
}

fn read_settings(paths: &AgyPaths) -> Result<Option<Map<String, Value>>> {
    let path = paths.settings();
    let Some(bytes) = fsutil::read_optional(&path)
        .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
    else {
        return Ok(None);
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        _ => Err(
            Error::format(format!("{} is not a JSON object", path.display()))
                .hint("agyva will not modify a settings file it cannot parse; fix or move it"),
        ),
    }
}

/// Fail early if `settings.json` exists but cannot be handled safely.
pub fn preflight(paths: &AgyPaths) -> Result<()> {
    read_settings(paths).map(|_| ())
}

/// Capture the live companion state. A malformed installation id is ignored
/// (with a log line) rather than copied.
pub fn capture(paths: &AgyPaths) -> Result<Companion> {
    let id_path = paths.installation_id();
    let installation_id = fsutil::read_optional(&id_path)
        .map_err(|e| Error::io(format!("reading {}", id_path.display()), e))?
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| {
            let n = normalize_installation_id(&s);
            if n.is_none() {
                tracing::warn!("installation_id is not a UUID; not captured");
            }
            n
        });
    let gcp = read_settings(paths)?.and_then(|m| m.get("gcp").cloned());
    Ok(Companion {
        installation_id,
        gcp,
    })
}

/// How [`apply`] treats absent values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Switching to an account: a missing installation id leaves the live one
    /// alone; a missing gcp block removes the live one (a consumer account
    /// must not inherit another account's GCP project).
    Switch,
    /// Rolling back to a checkpoint: absent means absent (remove).
    Restore { settings_existed: bool },
}

pub fn apply(paths: &AgyPaths, state: &Companion, mode: Mode) -> Result<()> {
    let id_path = paths.installation_id();
    match (&state.installation_id, mode) {
        (Some(id), _) => {
            if let Some(dir) = id_path.parent() {
                fsutil::create_dir_with_mode(dir)
                    .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
            }
            fsutil::atomic_write(&id_path, id.as_bytes())
                .map_err(|e| Error::io(format!("writing {}", id_path.display()), e))?;
        }
        (None, Mode::Restore { .. }) => {
            fsutil::remove_durable(&id_path)
                .map_err(|e| Error::io(format!("removing {}", id_path.display()), e))?;
        }
        (None, Mode::Switch) => {}
    }

    let settings_path = paths.settings();
    let existing = read_settings(paths)?;
    let mut map = match (&existing, &state.gcp) {
        (Some(m), _) => m.clone(),
        (None, Some(_)) => Map::new(),
        (None, None) => return Ok(()), // nothing to create, nothing to remove
    };
    match &state.gcp {
        Some(v) => {
            map.insert("gcp".into(), v.clone());
        }
        None => {
            if map.shift_remove("gcp").is_none() {
                return Ok(()); // already absent: do not rewrite the file
            }
        }
    }
    if map.is_empty()
        && matches!(
            mode,
            Mode::Restore {
                settings_existed: false
            }
        )
    {
        fsutil::remove_durable(&settings_path)
            .map_err(|e| Error::io(format!("removing {}", settings_path.display()), e))?;
        return Ok(());
    }
    let mut out = serde_json::to_vec_pretty(&Value::Object(map))
        .map_err(|e| Error::format(format!("cannot encode settings: {e}")))?;
    out.push(b'\n');
    let file_mode = fsutil::mode_of(&settings_path).unwrap_or(fsutil::PRIVATE_FILE_MODE);
    if let Some(dir) = settings_path.parent() {
        fsutil::create_dir_with_mode(dir)
            .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    }
    fsutil::atomic_write_mode(&settings_path, &out, file_mode)
        .map_err(|e| Error::io(format!("writing {}", settings_path.display()), e))
}

/// Whether `settings.json` currently exists (recorded in checkpoints).
pub fn settings_exist(paths: &AgyPaths) -> bool {
    paths.settings().exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, AgyPaths) {
        let d = tempfile::tempdir().unwrap();
        let p = AgyPaths {
            home: d.path().join("agy"),
        };
        (d, p)
    }

    #[test]
    fn only_gcp_block_is_touched() {
        let (_d, p) = paths();
        std::fs::create_dir_all(&p.home).unwrap();
        std::fs::write(
            p.settings(),
            r#"{"theme":"dark","n":1.50,"gcp":{"project":"old","location":"us"},"z":[1,2]}"#,
        )
        .unwrap();
        let state = Companion {
            installation_id: None,
            gcp: Some(serde_json::json!({"project":"new","location":"eu"})),
        };
        apply(&p, &state, Mode::Switch).unwrap();
        let text = std::fs::read_to_string(p.settings()).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["gcp"]["project"], "new");
        assert_eq!(v["theme"], "dark");
        assert!(text.contains("1.50"), "number formatting preserved: {text}");
        apply(&p, &Companion::default(), Mode::Switch).unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(p.settings()).unwrap()).unwrap();
        assert!(v.get("gcp").is_none());
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["z"], serde_json::json!([1, 2]));
    }

    #[test]
    fn unparseable_settings_fail_closed() {
        let (_d, p) = paths();
        std::fs::create_dir_all(&p.home).unwrap();
        std::fs::write(p.settings(), "{not json").unwrap();
        assert!(preflight(&p).is_err());
    }

    #[test]
    fn installation_id_validation() {
        assert!(normalize_installation_id("0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\n").is_some());
        assert!(normalize_installation_id("not-a-uuid").is_none());
    }
}
