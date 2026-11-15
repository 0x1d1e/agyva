//! Account selectors and aliases.
//!
//! Resolution order: `-` (previous), alias, 1-based number, exact email,
//! exact id, unique id prefix. Anything matching several accounts is an
//! error that lists the candidates.

use crate::core::error::{Error, ErrorKind, Result};
use crate::core::registry::{AccountRecord, Registry};

pub const MAX_ALIAS_LEN: usize = 32;

fn looks_like_id(s: &str) -> bool {
    s.len() == 10 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Aliases: 1-32 of `a-z 0-9 . _ -`, starting alphanumeric, not purely
/// digits and not shaped like an id (so they can never shadow another
/// selector kind).
pub fn validate_alias(alias: &str) -> Result<()> {
    let bad = |why: &str| {
        Err(Error::usage(format!("invalid alias `{alias}`: {why}"))
            .hint("use 1-32 lowercase letters, digits, `.`, `_` or `-`, starting with a letter or digit"))
    };
    if alias.is_empty() || alias.len() > MAX_ALIAS_LEN {
        return bad("length must be 1-32");
    }
    if !alias
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
    {
        return bad("only lowercase letters, digits, `.`, `_`, `-` are allowed");
    }
    if !alias.as_bytes()[0].is_ascii_alphanumeric() {
        return bad("must start with a letter or digit");
    }
    if alias.bytes().all(|b| b.is_ascii_digit()) {
        return bad("must not be only digits (numbers select by position)");
    }
    if looks_like_id(alias) {
        return bad("must not look like an account id");
    }
    Ok(())
}

/// Default alias from the email local part, de-duplicated with `-2`, `-3`...
pub fn derive_alias(email: Option<&str>, id: &str, registry: &Registry) -> String {
    let local = email.and_then(|e| e.split('@').next()).unwrap_or("");
    let mut base: String = local
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    base.truncate(MAX_ALIAS_LEN - 4);
    let base = base
        .trim_matches(|c| !char::is_ascii_alphanumeric(&c))
        .to_owned();
    let base = if validate_alias(&base).is_ok() {
        base
    } else {
        format!("acct-{}", &id[..id.len().min(6)])
    };
    if !registry.alias_taken(&base, None) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !registry.alias_taken(c, None))
        .unwrap_or(base)
}

fn describe(a: &AccountRecord, number: usize) -> String {
    format!(
        "  {number}. {} <{}> [{}]",
        a.alias,
        a.email.as_deref().unwrap_or("no email"),
        a.id
    )
}

fn ambiguous(registry: &Registry, input: &str, hits: &[usize]) -> Error {
    let lines: Vec<String> = hits
        .iter()
        .map(|&i| describe(&registry.accounts[i], i + 1))
        .collect();
    Error::new(
        ErrorKind::Ambiguous,
        format!("`{input}` matches several accounts:\n{}", lines.join("\n")),
    )
    .hint("use the alias, the list number, or a longer id")
}

pub fn resolve<'a>(registry: &'a Registry, input: &str) -> Result<&'a AccountRecord> {
    let by = |pred: &dyn Fn(&AccountRecord) -> bool| -> Vec<usize> {
        registry
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| pred(a))
            .map(|(i, _)| i)
            .collect()
    };
    let pick = |hits: Vec<usize>| -> Result<Option<&'a AccountRecord>> {
        match hits.as_slice() {
            [] => Ok(None),
            [i] => Ok(Some(&registry.accounts[*i])),
            _ => Err(ambiguous(registry, input, &hits)),
        }
    };
    if input == "-" {
        return registry
            .previous
            .as_deref()
            .and_then(|id| registry.find(id))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    "there is no previous account to switch back to",
                )
                .hint("`use -` works after you have switched at least once")
            });
    }
    let lower = input.to_ascii_lowercase();
    if let Some(a) = pick(by(&|a| a.alias.eq_ignore_ascii_case(input)))? {
        return Ok(a);
    }
    // A number within range is a list position; otherwise digits may still be
    // the start of an id (ids are hex), so fall through to the id match.
    let numeric = !input.is_empty() && input.bytes().all(|b| b.is_ascii_digit());
    if numeric {
        let hit = input
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| registry.accounts.get(i));
        if let Some(a) = hit {
            return Ok(a);
        }
    }
    if let Some(a) = pick(by(&|a| {
        a.email
            .as_deref()
            .is_some_and(|e| e.eq_ignore_ascii_case(input))
    }))? {
        return Ok(a);
    }
    if let Some(a) = pick(by(&|a| a.id == lower))? {
        return Ok(a);
    }
    if !lower.is_empty()
        && let Some(a) = pick(by(&|a| a.id.starts_with(&lower)))?
    {
        return Ok(a);
    }
    if numeric {
        return Err(
            Error::new(ErrorKind::NotFound, format!("no account number {input}")).hint(format!(
                "there are {} saved account(s); see `agyva list`",
                registry.accounts.len()
            )),
        );
    }
    Err(Error::new(
        ErrorKind::NotFound,
        format!("no saved account matches `{input}`"),
    )
    .hint("see `agyva list` for aliases, numbers and ids"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rec(id: &str, alias: &str, email: &str) -> AccountRecord {
        let now = Utc::now();
        AccountRecord {
            id: id.into(),
            alias: alias.into(),
            email: Some(email.into()),
            auth_method: "consumer".into(),
            added_at: now,
            updated_at: now,
            last_used_at: None,
        }
    }

    fn reg() -> Registry {
        Registry {
            accounts: vec![
                rec("aa11223344", "work", "w@example.com"),
                rec("aa99887766", "home", "h@example.com"),
                rec("bb00000000", "spare", "s@example.com"),
            ],
            previous: Some("bb00000000".into()),
            ..Registry::default()
        }
    }

    #[test]
    fn resolves_every_selector_kind() {
        let r = reg();
        assert_eq!(resolve(&r, "home").unwrap().id, "aa99887766");
        assert_eq!(resolve(&r, "HOME").unwrap().id, "aa99887766");
        assert_eq!(resolve(&r, "1").unwrap().id, "aa11223344");
        assert_eq!(resolve(&r, "s@example.com").unwrap().id, "bb00000000");
        assert_eq!(resolve(&r, "aa11223344").unwrap().id, "aa11223344");
        assert_eq!(resolve(&r, "bb").unwrap().id, "bb00000000");
        assert_eq!(resolve(&r, "-").unwrap().id, "bb00000000");
    }

    #[test]
    fn ambiguity_lists_candidates() {
        let err = resolve(&reg(), "aa").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Ambiguous);
        assert!(err.message().contains("work") && err.message().contains("home"));
    }

    #[test]
    fn missing_things_are_not_found() {
        let r = reg();
        for s in ["nope", "9", "0", "zz"] {
            assert_eq!(
                resolve(&r, s).unwrap_err().kind(),
                ErrorKind::NotFound,
                "{s}"
            );
        }
        let mut r = r;
        r.previous = None;
        assert_eq!(resolve(&r, "-").unwrap_err().kind(), ErrorKind::NotFound);
    }

    #[test]
    fn alias_rules() {
        for ok in ["work", "a.b_c-d", "x1"] {
            assert!(validate_alias(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "12",
            "UPPER",
            "a b",
            "a@b",
            "aa11223344",
            &"x".repeat(33),
        ] {
            assert!(validate_alias(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn default_alias_is_derived_and_deduplicated() {
        let mut r = Registry::default();
        assert_eq!(
            derive_alias(Some("Jane.Doe@example.com"), "aa11223344", &r),
            "jane.doe"
        );
        r.accounts.push(rec("aa11223344", "jane.doe", "j@x.com"));
        assert_eq!(
            derive_alias(Some("jane.doe@other.com"), "cc11223344", &r),
            "jane.doe-2"
        );
        assert_eq!(derive_alias(None, "cc11223344", &r), "acct-cc1122");
        assert_eq!(
            derive_alias(Some("123@x.com"), "cc11223344", &r),
            "acct-cc1122"
        );
    }
}
