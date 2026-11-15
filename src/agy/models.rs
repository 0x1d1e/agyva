//! Running `agy models` safely and parsing its output.
//!
//! The output is observed, undocumented and may change: lines are expected as
//! `<rawModelId>\t<Display Name>` (or space-separated), with informational
//! noise like "Fetching available models..." skipped. Parsing is defensive:
//! exactly this shape is understood, anything else is ignored — never guessed.
//!
//! Effort-suffixed ids (`<family>-high`, `-medium`, ...) are grouped into a
//! family id with the observed variants; a lone suffixed id stays standalone
//! unless it forms a family with at least two distinct efforts.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::agy::usage_run::{RunEnd, RunLimits};

pub const MODELS_ARGS: [&str; 1] = ["models"];

/// The effort levels agy appends to model family ids, in agy's own order.
pub const EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// Maximum parsed entries kept (a real catalog holds dozens; the bound keeps
/// a hostile output from ballooning memory).
const MAX_ENTRIES: usize = 512;
const MAX_ID_LEN: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawModelEntry {
    pub raw_model_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFamily {
    /// The family id (`gemini-4-carbon`) or a standalone raw id.
    pub id: String,
    pub display_name: String,
    /// Observed effort variants, in `EFFORTS` order (empty for standalone).
    pub variants: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub models: Vec<ModelFamily>,
}

/// Parse `agy models` output. Noise lines are skipped; a line without a
/// separator, an over-long id or a non-UTF8-shaped entry is ignored.
pub fn parse_models(output: &str) -> ModelCatalog {
    let mut raw: Vec<RawModelEntry> = Vec::new();
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty()
            || line.starts_with("Fetching available models")
            || line.starts_with("Fetching ")
            || line.starts_with('#')
        {
            continue;
        }
        let Some((id, name)) = line.split_once('\t').or_else(|| line.split_once("  ")) else {
            continue;
        };
        let id = id.trim();
        let name = name.trim();
        if id.is_empty()
            || name.is_empty()
            || id.len() > MAX_ID_LEN
            || id.contains(char::is_whitespace)
        {
            continue;
        }
        if raw.len() < MAX_ENTRIES {
            raw.push(RawModelEntry {
                raw_model_id: id.to_owned(),
                display_name: crate::core::format::clean_name(name),
            });
        }
    }
    build_catalog(&raw)
}

fn effort_suffix(id: &str) -> Option<(&str, &str)> {
    let (base, suffix) = id.rsplit_once('-')?;
    let suffix = suffix.to_ascii_lowercase();
    EFFORTS
        .iter()
        .find(|e| **e == suffix)
        .map(|e| (base, *e))
        .filter(|(base, _)| !base.is_empty())
}

/// A display name without its effort suffix ("Gemini 4 Carbon (High)").
fn strip_effort_name(name: &str) -> &str {
    let trimmed = name.trim();
    let Some(open) = trimmed.rfind(" (") else {
        return trimmed;
    };
    let inner = &trimmed[open + 2..];
    if inner
        .strip_suffix(')')
        .is_some_and(|e| EFFORTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
    {
        trimmed[..open].trim()
    } else {
        trimmed
    }
}

fn build_catalog(raw: &[RawModelEntry]) -> ModelCatalog {
    // Family id -> display name of the first variant seen + efforts observed.
    let mut families: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut standalone: Vec<&RawModelEntry> = Vec::new();

    for entry in raw {
        if let Some((base, effort)) = effort_suffix(&entry.raw_model_id) {
            let eff = effort.to_owned();
            if let Some(i) = families.iter().position(|(id, _, _)| id == base) {
                if !families[i].2.contains(&eff) {
                    families[i].2.push(eff);
                }
            } else {
                families.push((base.to_owned(), entry.display_name.clone(), vec![eff]));
            }
        } else {
            standalone.push(entry);
        }
    }

    let mut models: Vec<ModelFamily> = Vec::new();
    for (id, name, efforts) in families {
        // A family needs at least two distinct efforts; a lone suffixed id
        // (an id that merely ends in an effort word) is kept standalone with
        // its full raw id.
        if efforts.len() >= 2 {
            let mut sorted = efforts;
            sorted.sort_by_key(|e| EFFORTS.iter().position(|x| x == e).unwrap_or(EFFORTS.len()));
            models.push(ModelFamily {
                id,
                display_name: strip_effort_name(&name).to_owned(),
                variants: sorted,
            });
        } else {
            models.push(ModelFamily {
                id: format!("{id}-{}", efforts[0]),
                display_name: name,
                variants: Vec::new(),
            });
        }
    }
    for entry in &standalone {
        if !models.iter().any(|m| m.id == entry.raw_model_id) {
            models.push(ModelFamily {
                id: entry.raw_model_id.clone(),
                display_name: entry.display_name.clone(),
                variants: Vec::new(),
            });
        }
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    ModelCatalog { models }
}

/// What running `agy models` produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelsRun {
    /// Parsed catalog (possibly empty if the output held nothing usable).
    Ok(ModelCatalog),
    /// agy ran but did not succeed (nonzero exit, timeout, no usable text).
    Failed(String),
}

/// Run `agy models` under the same limits and hygiene as `/usage`: no stdin,
/// no `AGYVA_*` environment, bounded output, killed on timeout.
pub fn run_models(bin: &Path, limits: &RunLimits) -> std::io::Result<ModelsRun> {
    let out = run_args(bin, MODELS_ARGS, limits)?;
    let (exit_code, timed_out) = match out.end {
        RunEnd::Exited(c) => (c, false),
        RunEnd::TimedOut => (None, true),
    };
    if timed_out {
        return Ok(ModelsRun::Failed(
            "agy models did not answer in time".into(),
        ));
    }
    if exit_code != Some(0) {
        return Ok(ModelsRun::Failed(format!(
            "agy models exited with status {exit_code:?}"
        )));
    }
    let catalog = parse_models(&out.stdout);
    if catalog.models.is_empty() {
        return Ok(ModelsRun::Failed(
            "agy models printed no recognizable model lines".into(),
        ));
    }
    Ok(ModelsRun::Ok(catalog))
}

/// Same child hygiene as `run_usage` but with caller-chosen arguments (used
/// only for `models`, which shares every safety rule).
pub(crate) fn run_args(
    bin: &Path,
    args: [&str; 1],
    limits: &RunLimits,
) -> std::io::Result<crate::agy::usage_run::RunOutput> {
    // Delegate: identical spawn/read/kill discipline, different argv.
    crate::agy::usage_run::run_with_args(bin, &args, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tsv_with_noise_and_families() {
        let out = "\
Fetching available models...\n\
gemini-4-carbon-high\tGemini 4 Carbon (High)\n\
gemini-4-carbon-medium\tGemini 4 Carbon (Medium)\n\
gemini-4-carbon-low\tGemini 4 Carbon (Low)\n\
gemini-pro-orchestrator\tOrchestrator\n\
not-a-model-line\n\
gpt-oss-120b\tGPT-OSS 120B\n";
        let c = parse_models(out);
        let carbon = c.models.iter().find(|m| m.id == "gemini-4-carbon").unwrap();
        assert_eq!(carbon.display_name, "Gemini 4 Carbon");
        assert_eq!(carbon.variants, vec!["low", "medium", "high"]);
        let orch = c
            .models
            .iter()
            .find(|m| m.id == "gemini-pro-orchestrator")
            .unwrap();
        assert!(orch.variants.is_empty());
        assert!(c.models.iter().any(|m| m.id == "gpt-oss-120b"));
        assert_eq!(c.models.len(), 3);
    }

    #[test]
    fn lone_effort_suffix_is_standalone() {
        let out = "gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n";
        let c = parse_models(out);
        assert_eq!(c.models.len(), 1);
        assert_eq!(c.models[0].id, "gpt-oss-120b-medium");
        assert!(c.models[0].variants.is_empty());
    }

    #[test]
    fn space_separated_lines_and_bounds() {
        let out = "model-a  Model A Display\nmodel-b\tModel B\n";
        let c = parse_models(out);
        assert_eq!(c.models.len(), 2);
        assert!(
            parse_models("Fetching available models...")
                .models
                .is_empty()
        );
        let long = format!("{}\tName", "x".repeat(129));
        assert!(parse_models(&long).models.is_empty());
    }
}
