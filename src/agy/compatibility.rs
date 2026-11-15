//! Version-aware compatibility gate. Known-good: agy 1.5.x. Anything else
//! fails closed unless the user opts in with `--allow-untested` (or the
//! `allow_untested_agy` config key); credential-shape validation still applies.

use std::fmt;

use crate::core::error::{Error, ErrorKind, Result};

pub const KNOWN_GOOD: &str = "1.5.x";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgyVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for AgyVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Extract the first `X.Y.Z` token from `agy --version` output.
pub fn parse_version(output: &str) -> Option<AgyVersion> {
    output
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .find_map(|tok| {
            let tok = tok.trim_start_matches('v');
            let core = tok.split(['-', '+']).next()?;
            let mut it = core.split('.');
            let major = it.next()?.parse().ok()?;
            let minor = it.next()?.parse().ok()?;
            let patch = it.next()?.parse().ok()?;
            it.next().is_none().then_some(AgyVersion {
                major,
                minor,
                patch,
            })
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compat {
    Supported(AgyVersion),
    Untested(AgyVersion),
    /// Could not determine the version (agy missing or unparseable output).
    Unknown(String),
}

impl Compat {
    pub fn assess(detected: std::result::Result<AgyVersion, String>) -> Self {
        match detected {
            Ok(v) if v.major == 1 && v.minor == 5 => Self::Supported(v),
            Ok(v) => Self::Untested(v),
            Err(why) => Self::Unknown(why),
        }
    }

    pub fn version_string(&self) -> Option<String> {
        match self {
            Self::Supported(v) | Self::Untested(v) => Some(v.to_string()),
            Self::Unknown(_) => None,
        }
    }

    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported(_))
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Supported(v) => format!("agy {v} (supported)"),
            Self::Untested(v) => format!("agy {v} (untested; known-good is {KNOWN_GOOD})"),
            Self::Unknown(why) => format!("agy version unknown ({why})"),
        }
    }

    /// Gate for operations that read or write agy's credential format.
    pub fn require(&self, allow_untested: bool) -> Result<()> {
        match self {
            Self::Supported(_) => Ok(()),
            _ if allow_untested => {
                tracing::warn!("proceeding with {} because untested agy versions are allowed", self.summary());
                Ok(())
            }
            other => Err(Error::new(
                ErrorKind::Incompatible,
                format!("refusing to continue: {}", other.summary()),
            )
            .hint(
                "agyva only writes credential formats it was verified against. If you accept the risk, \
                 rerun with --allow-untested (or `agyva config set allow_untested_agy true`)",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_shapes() {
        for (s, want) in [
            ("1.5.9", (1, 5, 9)),
            ("agy version 1.5.9\n", (1, 5, 9)),
            ("Antigravity CLI v1.6.0 (build abc)", (1, 6, 0)),
            ("agy 2.0.1-beta.1", (2, 0, 1)),
        ] {
            let v = parse_version(s).unwrap_or_else(|| panic!("parse {s}"));
            assert_eq!((v.major, v.minor, v.patch), want);
        }
        assert!(parse_version("no version here").is_none());
        assert!(parse_version("1.5").is_none());
    }

    #[test]
    fn gate_fails_closed() {
        let ok = Compat::assess(Ok(AgyVersion {
            major: 1,
            minor: 5,
            patch: 9,
        }));
        assert!(ok.require(false).is_ok());
        let newer = Compat::assess(Ok(AgyVersion {
            major: 1,
            minor: 6,
            patch: 0,
        }));
        assert!(newer.require(false).is_err());
        assert!(newer.require(true).is_ok());
        let unknown = Compat::assess(Err("agy not found".into()));
        assert!(unknown.require(false).is_err());
    }
}
