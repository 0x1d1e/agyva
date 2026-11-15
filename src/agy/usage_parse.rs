//! Parser for `agy -p "/usage" --output-format json`.
//!
//! The output is observed, undocumented and may change. Parsing is
//! defensive: it understands exactly the shape described below, tolerates
//! partial data inside it, and reports anything else as a schema change.
//! It never guesses a value.
//!
//! ```text
//! { status, response, usage{..},
//!   command: { data: { groups: [ { name, description, disabled?,
//!       buckets: [ { id, name, window: "weekly"|"5h",
//!                    remaining_fraction: 0..1, reset_time: ISO8601 } ] } ] } } }
//! ```
//!
//! `command` is read from the top level, or from `usage.command` if the
//! top level has none.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::core::quota::{ModelQuota, QuotaSource, QuotaWindow};

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedUsage {
    pub groups: Vec<ModelQuota>,
    /// Buckets or fields skipped because they were malformed.
    pub ignored: usize,
}

/// Why the output could not be used. The text never contains agy output.
pub type SchemaError = String;

fn json_of(stdout: &str) -> Option<Value> {
    let s = stdout.trim();
    serde_json::from_str(s).ok().or_else(|| {
        // Tolerate a banner or log line around the object.
        let (a, b) = (s.find('{')?, s.rfind('}')?);
        serde_json::from_str(s.get(a..=b)?).ok()
    })
}

/// True when the group is explicitly marked disabled.
fn is_disabled(g: &Value) -> bool {
    g.get("disabled").and_then(Value::as_bool) == Some(true)
        || g.get("enabled").and_then(Value::as_bool) == Some(false)
        || ["status", "state"].iter().any(|k| {
            g.get(*k)
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("disabled"))
        })
}

fn groups_of(root: &Value) -> Option<&Vec<Value>> {
    root.pointer("/command/data/groups")
        .or_else(|| root.pointer("/usage/command/data/groups"))
        .and_then(Value::as_array)
}

fn window_of(
    bucket: &Value,
    observed_at: DateTime<Utc>,
    ignored: &mut usize,
) -> Option<(bool, QuotaWindow)> {
    let five = match bucket.get("window").and_then(Value::as_str) {
        Some("5h") => true,
        Some("weekly") => false,
        _ => {
            *ignored += 1;
            return None;
        }
    };
    let Some(frac) = bucket
        .get("remaining_fraction")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
    else {
        *ignored += 1;
        return None;
    };
    let reset_at = bucket
        .get("reset_time")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc));
    Some((
        five,
        QuotaWindow {
            remaining_percent: frac * 100.0,
            reset_at,
            observed_at,
            source: QuotaSource::AgyUsageCommand,
        },
    ))
}

pub fn parse_usage(stdout: &str, observed_at: DateTime<Utc>) -> Result<ParsedUsage, SchemaError> {
    let root = json_of(stdout).ok_or("output is not JSON")?;
    let groups = groups_of(&root).ok_or("no command.data.groups array")?;
    let mut out = Vec::new();
    let mut ignored = 0usize;
    for g in groups {
        let Some(name) = g
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
        else {
            return Err("a quota group has no name".into());
        };
        let disabled = is_disabled(g);
        let buckets: &[Value] = match g.get("buckets").and_then(Value::as_array) {
            Some(b) => b,
            None => {
                ignored += 1;
                &[]
            }
        };
        let mut primary = ModelQuota {
            group: crate::core::format::clean_name(name),
            model: None,
            disabled,
            five_hour: None,
            weekly: None,
        };
        let mut extra: Vec<ModelQuota> = Vec::new();
        for b in buckets {
            let Some((five, w)) = window_of(b, observed_at, &mut ignored) else {
                continue;
            };
            let slot = if five {
                &mut primary.five_hour
            } else {
                &mut primary.weekly
            };
            if slot.is_none() {
                *slot = Some(w);
                continue;
            }
            // A second bucket for the same window: keep it, labelled by name.
            let label = ["name", "id"]
                .iter()
                .find_map(|k| b.get(*k).and_then(Value::as_str))
                .map(str::to_owned);
            let mut q = ModelQuota {
                group: name.to_owned(),
                model: label,
                disabled,
                five_hour: None,
                weekly: None,
            };
            if five {
                q.five_hour = Some(w);
            } else {
                q.weekly = Some(w);
            }
            extra.push(q);
        }
        out.push(primary);
        out.extend(extra);
    }
    if out.is_empty() {
        return Err("the groups list is empty".into());
    }
    if out
        .iter()
        .all(|q| !q.disabled && q.windows().next().is_none())
    {
        return Err("no usable quota windows".into());
    }
    Ok(ParsedUsage {
        groups: out,
        ignored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn doc(groups: Value) -> String {
        json!({"status":"success","response":"","usage":{},"command":{"data":{"groups":groups}}})
            .to_string()
    }

    #[test]
    fn parses_groups_windows_and_disabled() {
        let s = doc(json!([
            {"name":"Gemini","description":"d","buckets":[
                {"id":"a","name":"5h","window":"5h","remaining_fraction":0.72,"reset_time":"2030-01-01T05:00:00Z"},
                {"id":"b","name":"wk","window":"weekly","remaining_fraction":0.4,"reset_time":"2030-01-05T00:00:00Z"}]},
            {"name":"Claude","disabled":true,"buckets":[
                {"id":"c","name":"5h","window":"5h","remaining_fraction":0}]}
        ]));
        let p = parse_usage(&s, now()).expect("parse");
        assert_eq!(p.groups.len(), 2);
        let g = &p.groups[0];
        assert!((g.five_hour.as_ref().unwrap().remaining_percent - 72.0).abs() < 1e-9);
        assert!(g.weekly.as_ref().unwrap().reset_at.is_some());
        assert!(p.groups[1].disabled && !p.groups[1].exhausted());
    }

    #[test]
    fn partial_data_is_kept_and_counted() {
        let s = doc(json!([{"name":"G","buckets":[
            {"window":"5h","remaining_fraction":0.5},
            {"window":"weekly","remaining_fraction":1.5},
            {"window":"daily","remaining_fraction":0.5},
            {"window":"weekly"}]}]));
        let p = parse_usage(&s, now()).expect("parse");
        assert!(p.groups[0].five_hour.as_ref().unwrap().reset_at.is_none());
        assert!(p.groups[0].weekly.is_none());
        assert_eq!(p.ignored, 3);
    }

    #[test]
    fn unknown_shapes_are_errors() {
        for s in [
            "",
            "not json",
            "{{{",
            "[]",
            r#"{"status":"success"}"#,
            &doc(json!([])),
            &doc(json!([{"buckets":[]}])),
            &doc(json!([{"name":"G","buckets":[{"window":"daily","remaining_fraction":0.5}]}])),
            &doc(json!({"not":"an array"})),
        ] {
            assert!(parse_usage(s, now()).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn duplicate_window_buckets_become_separate_entries() {
        let s = doc(json!([{"name":"G","buckets":[
            {"name":"m1","window":"5h","remaining_fraction":0.5},
            {"name":"m2","window":"5h","remaining_fraction":0.1}]}]));
        let p = parse_usage(&s, now()).expect("parse");
        assert_eq!(p.groups.len(), 2);
        assert_eq!(p.groups[1].model.as_deref(), Some("m2"));
    }

    #[test]
    fn banner_around_the_object_is_tolerated() {
        let s = format!(
            "starting...\n{}\n",
            doc(json!([{"name":"G","buckets":[{"window":"5h","remaining_fraction":0.5}]}]))
        );
        assert!(parse_usage(&s, now()).is_ok());
    }
}
