mod common;

use std::time::Duration as StdDuration;

use agyva::core::monitor::MonitorProbe;
use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};
use agyva::core::quota_history::{
    MAX_SAMPLES_PER_SERIES, QuotaHistory, QuotaSample, compute_burn_rate,
};
use agyva::core::usage::{RefreshMode, UsageOptions};
use agyva::core::watch::Probe;
use chrono::{Duration, TimeZone, Utc};
use common::fake_agy::FakeAgy;
use common::*;

fn sample(
    t: chrono::DateTime<Utc>,
    pct: f64,
    reset_at: Option<chrono::DateTime<Utc>>,
) -> QuotaSample {
    QuotaSample {
        observed_at: t,
        remaining_percent: pct,
        reset_at,
    }
}

#[test]
fn burn_rate_calculates_eta_and_resets_only_from_post_reset_segment() {
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let reset = t0 + Duration::hours(5);

    // Pre-reset segment drops 80 -> 20, then jumps +75 (reset!) to 95, then drops 95 -> 75 over 1h
    let samples = vec![
        sample(t0, 80.0, Some(t0 + Duration::minutes(30))),
        sample(
            t0 + Duration::minutes(20),
            20.0,
            Some(t0 + Duration::minutes(30)),
        ),
        sample(t0 + Duration::minutes(60), 95.0, Some(reset)),
        sample(t0 + Duration::minutes(120), 75.0, Some(reset)),
    ];
    let now = t0 + Duration::minutes(120);
    let br = compute_burn_rate(&samples, Some(reset), now).unwrap();
    assert_eq!(br.samples, 2, "only post-reset segment counts");
    let rate = br.percent_per_hour.unwrap();
    assert!((rate - 20.0).abs() < 1e-6, "rate={rate}");
    // 75% remaining at 20%/h = 3.75h = 13500s; reset is 3h away (10800s) -> exhausts after reset
    assert_eq!(br.eta_exhaustion_secs, Some(13_500));
    assert_eq!(br.exhausts_before_reset, Some(false));

    // Fast burn: 95 -> 35 in 1h (60%/h), 35% left -> 2100s ETA, reset in 3h -> exhausts_before_reset = true
    let fast = vec![
        sample(t0, 95.0, Some(reset)),
        sample(t0 + Duration::hours(1), 35.0, Some(reset)),
    ];
    let br_fast = compute_burn_rate(&fast, Some(reset), t0 + Duration::hours(1)).unwrap();
    assert_eq!(br_fast.eta_exhaustion_secs, Some(2100));
    assert_eq!(br_fast.exhausts_before_reset, Some(true));
}

#[test]
fn quota_history_caps_at_64_and_populates_usage_report_and_monitor_snapshot() {
    let rig = Rig::new();
    let fake = FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    rig.add_all(&[ALICE]);

    let hist = QuotaHistory::new(&rig.dirs());
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for i in 0..80 {
        let t = t0 + Duration::minutes(i);
        let pct = 100.0 - (i as f64 * 0.5);
        hist.record_refreshed(
            &id_of(ALICE),
            &[ModelQuota {
                group: "Gemini Pro".into(),
                model: None,
                disabled: false,
                five_hour: Some(QuotaWindow {
                    remaining_percent: pct,
                    reset_at: Some(t0 + Duration::hours(5)),
                    observed_at: t,
                    source: QuotaSource::AgyUsageCommand,
                }),
                weekly: None,
            }],
            t,
            &[id_of(ALICE)],
        )
        .unwrap();
    }

    let loaded = hist.load();
    let series = &loaded.accounts[&id_of(ALICE)]["Gemini Pro"].five_hour;
    assert_eq!(series.len(), MAX_SAMPLES_PER_SERIES);
    assert_eq!(series[0].observed_at, t0 + Duration::minutes(16));

    // Corrupt file degrades cleanly to empty
    std::fs::write(hist.path(), "{{corrupt").unwrap();
    assert!(hist.load().accounts.is_empty());

    // Live refresh records into quota-history.json and UsageReport exposes burn_rate
    let app = rig.app_with_agy(&fake);
    fake.ok(0.80, 0.90);
    app.refresh_active_at(true, t0).unwrap();
    fake.ok(0.60, 0.85);
    app.refresh_active_at(true, t0 + Duration::hours(1))
        .unwrap();

    let report = app
        .usage_report(&UsageOptions {
            selector: None,
            all: true,
            refresh: RefreshMode::Skip,
        })
        .unwrap();
    let five = report.accounts[0].groups[0].five_hour.as_ref().unwrap();
    let br = five.burn_rate.as_ref().unwrap();
    assert_eq!(br.samples, 2);
    assert!((br.percent_per_hour.unwrap() - 20.0).abs() < 1e-6);

    // MonitorProbe snapshot exposes burn_rate & eta_exhaustion_secs
    let mut probe = MonitorProbe::new(&app, None, None, StdDuration::from_secs(300));
    probe.cycle(t0 + Duration::hours(1));
    let snap = probe.last_snapshot().unwrap();
    assert_eq!(snap.rows.len(), 1);
    assert!(snap.rows[0].burn_rate.is_some());
    assert!(snap.rows[0].eta_exhaustion_secs.is_some());
}
