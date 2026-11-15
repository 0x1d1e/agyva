//! Shape checks for identifiers that end up in file names.
//!
//! Registry and journal files are plain JSON that a user (or another tool)
//! could edit; an id read from them must never be able to steer a path.

use crate::core::error::{Error, Result};

fn lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Account ids are the first 10 hex characters of SHA-256(sub).
pub fn is_account_id(s: &str) -> bool {
    lower_hex(s, 10)
}

/// Transaction ids are 6 random bytes, hex encoded.
pub fn is_txn_id(s: &str) -> bool {
    lower_hex(s, 12)
}

pub fn require_account_id(s: &str) -> Result<()> {
    if is_account_id(s) {
        Ok(())
    } else {
        Err(Error::format(
            "an account id is malformed (expected 10 lowercase hex characters)",
        ))
    }
}

pub fn require_txn_id(s: &str) -> Result<()> {
    if is_txn_id(s) {
        Ok(())
    } else {
        Err(Error::format(
            "a transaction id is malformed (expected 12 lowercase hex characters)",
        ))
    }
}

/// Checkpoint file names: `<timestamp>-<txn>[.keep].ckpt`, no separators.
pub fn is_checkpoint_name(s: &str) -> bool {
    s.len() <= 96
        && s.ends_with(".ckpt")
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_ids_are_exactly_ten_lowercase_hex() {
        assert!(is_account_id("0123456789"));
        assert!(is_account_id("abcdef0123"));
        for bad in [
            "",
            "abc",
            "ABCDEF0123",
            "../../etc/",
            "0123456789a",
            "012345678g",
            "0123456 89",
        ] {
            assert!(!is_account_id(bad), "{bad}");
        }
    }

    #[test]
    fn checkpoint_names_cannot_escape() {
        assert!(is_checkpoint_name(
            "20260101T000000.000001Z-0123456789ab.ckpt"
        ));
        assert!(is_checkpoint_name(
            "20260101T000000.000001Z-0123456789ab.keep.ckpt"
        ));
        for bad in [
            "../x.ckpt",
            "a/b.ckpt",
            ".hidden.ckpt",
            "x.txt",
            "a\\b.ckpt",
            "",
        ] {
            assert!(!is_checkpoint_name(bad), "{bad}");
        }
    }
}
