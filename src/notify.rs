//! Optional desktop notifications for `agyva watch` (off by default).
//!
//! Failing to notify (no session bus, no daemon, headless server) is never
//! an error: it is logged once and notifications stop for the run.

use std::cell::Cell;

pub trait Notifier {
    fn notify(&self, title: &str, body: &str);
}

/// Does nothing; used when notifications are disabled.
pub struct Silent;

impl Notifier for Silent {
    fn notify(&self, _title: &str, _body: &str) {}
}

pub struct Desktop {
    broken: Cell<bool>,
}

impl Desktop {
    pub fn new() -> Self {
        Self {
            broken: Cell::new(false),
        }
    }
}

impl Default for Desktop {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier for Desktop {
    fn notify(&self, title: &str, body: &str) {
        if self.broken.get() {
            return;
        }
        let sent = notify_rust::Notification::new()
            .appname("agyva")
            .summary(title)
            .body(body)
            .show();
        if let Err(e) = sent {
            self.broken.set(true);
            tracing::warn!("desktop notifications are unavailable ({e}); continuing without them");
        }
    }
}
