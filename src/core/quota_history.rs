//! Bounded quota observation history (`<cache>/quota-history.json`) and
//! burn-rate analytics.
//!
//! Stores up to [`MAX_SAMPLES_PER_SERIES`] timestamped remaining-percentage
//! samples per `(account_id, group_label, window)` under a cache lock with 0600
//! atomic writes. Read failures are lenient (corrupt/oversized/foreign files
//! degrade to empty history).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::error::{Error, Result};
use crate::core::quota::ModelQuota;
use crate::paths::Dirs;
use crate::storage::fsutil;
use crate::storage::lock::StateLock;

pub const QUOTA_HISTORY_SCHEMA: u32 = 1;
pub const MAX_SAMPLES_PER_SERIES: usize = 64;
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const RESET_JUMP_THRESHOLD_PERCENT: f64 = 5.0;

pub fn history_file(dirs: &Dirs) -> PathBuf {
    dirs.cache.join("quota-history.json")
}

fn history_lock_file(dirs: &Dirs) -> PathBuf {
    dirs.cache.join("quota-history.lock")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSample {
    pub observed_at: DateTime<Utc>,
    pub remaining_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupWindowHistory {
    pub five_hour: Vec<QuotaSample>,
    pub weekly: Vec<QuotaSample>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaHistoryFile {
    pub schema_version: u32,
    /// `account_id -> group_label -> { five_hour, weekly }`
    pub accounts: BTreeMap<String, BTreeMap<String, GroupWindowHistory>>,
}

impl Default for QuotaHistoryFile {
    fn default() -> Self {
        Self {
            schema_version: QUOTA_HISTORY_SCHEMA,
            accounts: BTreeMap::new(),
        }
    }
}

impl QuotaHistoryFile {
    fn sanitize(&mut self) {
        for groups in self.accounts.values_mut() {
            for hist in groups.values_mut() {
                sanitize_series(&mut hist.five_hour);
                sanitize_series(&mut hist.weekly);
            }
        }
    }
}

fn sanitize_series(samples: &mut Vec<QuotaSample>) {
    samples.retain(|s| {
        s.remaining_percent.is_finite() && (0.0..=100.0).contains(&s.remaining_percent)
    });
    samples.sort_by_key(|s| s.observed_at);
    samples.dedup_by(|later, earlier| {
        if later.observed_at == earlier.observed_at {
            *earlier = later.clone();
            true
        } else {
            false
        }
    });
    if samples.len() > MAX_SAMPLES_PER_SERIES {
        let excess = samples.len() - MAX_SAMPLES_PER_SERIES;
        samples.drain(..excess);
    }
}

fn push_sample(samples: &mut Vec<QuotaSample>, sample: QuotaSample) {
    if !sample.remaining_percent.is_finite() || !(0.0..=100.0).contains(&sample.remaining_percent) {
        return;
    }
    if let Some(existing) = samples
        .iter_mut()
        .find(|s| s.observed_at == sample.observed_at)
    {
        *existing = sample;
    } else {
        samples.push(sample);
    }
    sanitize_series(samples);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BurnRateView {
    pub percent_per_hour: Option<f64>,
    pub eta_exhaustion_secs: Option<u64>,
    pub exhausts_before_reset: Option<bool>,
    pub samples: usize,
}

/// Compute burn rate from the latest post-reset segment of `samples`.
/// Returns `None` when no samples exist.
pub fn compute_burn_rate(
    samples: &[QuotaSample],
    current_reset_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<BurnRateView> {
    if samples.is_empty() {
        return None;
    }
    let mut start = 0usize;
    for i in 1..samples.len() {
        if samples[i].remaining_percent - samples[i - 1].remaining_percent
            > RESET_JUMP_THRESHOLD_PERCENT
        {
            start = i;
        }
    }
    let seg = &samples[start..];
    if seg.len() < 2 {
        return Some(BurnRateView {
            percent_per_hour: None,
            eta_exhaustion_secs: None,
            exhausts_before_reset: None,
            samples: seg.len(),
        });
    }
    let first = &seg[0];
    let last = &seg[seg.len() - 1];
    let elapsed_secs = (last.observed_at - first.observed_at).num_seconds();
    if elapsed_secs <= 0 {
        return Some(BurnRateView {
            percent_per_hour: None,
            eta_exhaustion_secs: None,
            exhausts_before_reset: None,
            samples: seg.len(),
        });
    }
    let dropped = first.remaining_percent - last.remaining_percent;
    let reset = current_reset_at.or(last.reset_at);
    if dropped <= 0.0 {
        return Some(BurnRateView {
            percent_per_hour: Some(0.0),
            eta_exhaustion_secs: None,
            exhausts_before_reset: reset.map(|_| false),
            samples: seg.len(),
        });
    }
    let elapsed_hours = elapsed_secs as f64 / 3600.0;
    let rate = dropped / elapsed_hours;
    if !rate.is_finite() || rate <= 0.0 {
        return Some(BurnRateView {
            percent_per_hour: None,
            eta_exhaustion_secs: None,
            exhausts_before_reset: None,
            samples: seg.len(),
        });
    }
    let eta_secs = if last.remaining_percent <= 0.0 {
        Some(0)
    } else {
        let secs = (last.remaining_percent / rate) * 3600.0;
        if secs.is_finite() && secs >= 0.0 {
            Some(secs.min(u64::MAX as f64).round() as u64)
        } else {
            None
        }
    };
    let exhausts_before_reset = match (eta_secs, reset) {
        (Some(eta), Some(r)) => {
            let until_reset = (r - last.observed_at).num_seconds();
            if until_reset > 0 {
                Some(eta < until_reset as u64)
            } else {
                let until_now = (r - now).num_seconds();
                Some(until_now > 0 && eta < until_now as u64)
            }
        }
        _ => None,
    };
    Some(BurnRateView {
        percent_per_hour: Some(rate),
        eta_exhaustion_secs: eta_secs,
        exhausts_before_reset,
        samples: seg.len(),
    })
}

#[derive(Debug, Clone)]
pub struct QuotaHistory {
    file: PathBuf,
    lock: PathBuf,
}

impl QuotaHistory {
    pub fn new(dirs: &Dirs) -> Self {
        Self {
            file: history_file(dirs),
            lock: history_lock_file(dirs),
        }
    }

    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn load(&self) -> QuotaHistoryFile {
        let bytes = match fsutil::read_optional(&self.file) {
            Ok(Some(b)) if b.len() <= MAX_HISTORY_BYTES => b,
            Ok(Some(_)) => {
                tracing::warn!("quota history is oversized; ignoring it");
                return QuotaHistoryFile::default();
            }
            Ok(None) => return QuotaHistoryFile::default(),
            Err(e) => {
                tracing::warn!("cannot read quota history: {e}");
                return QuotaHistoryFile::default();
            }
        };
        match serde_json::from_slice::<QuotaHistoryFile>(&bytes) {
            Ok(mut f) if f.schema_version == QUOTA_HISTORY_SCHEMA => {
                f.sanitize();
                f
            }
            _ => {
                tracing::warn!("quota history is unreadable or foreign; ignoring it");
                QuotaHistoryFile::default()
            }
        }
    }

    pub fn update(&self, change: impl FnOnce(&mut QuotaHistoryFile)) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        let _guard = StateLock::acquire(&self.lock, Duration::from_secs(3))?;
        let mut file = self.load();
        change(&mut file);
        file.sanitize();
        let mut bytes =
            serde_json::to_vec_pretty(&file).map_err(|e| Error::format(e.to_string()))?;
        bytes.push(b'\n');
        fsutil::atomic_write(&self.file, &bytes)
            .map_err(|e| Error::io(format!("writing {}", self.file.display()), e))
    }

    /// Record a refreshed quota reading for `account_id`, retaining only valid
    /// `keep_account_ids` in the history file.
    pub fn record_refreshed(
        &self,
        account_id: &str,
        groups: &[ModelQuota],
        observed_at: DateTime<Utc>,
        keep_account_ids: &[String],
    ) -> Result<()> {
        self.update(|f| {
            f.accounts.retain(|k, _| keep_account_ids.contains(k));
            let acct = f.accounts.entry(account_id.to_owned()).or_default();
            for q in groups.iter().filter(|g| !g.disabled) {
                let label = q.label();
                let entry = acct.entry(label).or_default();
                if let Some(w) = &q.five_hour {
                    push_sample(
                        &mut entry.five_hour,
                        QuotaSample {
                            observed_at,
                            remaining_percent: w.remaining_percent,
                            reset_at: w.reset_at,
                        },
                    );
                }
                if let Some(w) = &q.weekly {
                    push_sample(
                        &mut entry.weekly,
                        QuotaSample {
                            observed_at,
                            remaining_percent: w.remaining_percent,
                            reset_at: w.reset_at,
                        },
                    );
                }
            }
        })
    }
}
