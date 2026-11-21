//! A copy of the fake `agy` script in a tempdir, steered through files.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub struct FakeAgy {
    dir: PathBuf,
}

/// Synthetic `/usage` output: one enabled group plus a disabled one.
pub fn usage_json(five: f64, weekly: f64) -> String {
    format!(
        r#"{{"status":"ok","response":"","usage":{{}},"command":{{"data":{{"groups":[
{{"name":"Gemini Pro","description":"synthetic","buckets":[
 {{"id":"g5","name":"Gemini 5h","window":"5h","remaining_fraction":{five},"reset_time":"2099-01-01T05:00:00Z"}},
 {{"id":"gw","name":"Gemini weekly","window":"weekly","remaining_fraction":{weekly},"reset_time":"2099-01-07T00:00:00Z"}}]}},
{{"name":"Claude / GPT","description":"synthetic","disabled":true,"buckets":[
 {{"id":"c5","name":"Claude 5h","window":"5h","remaining_fraction":0,"reset_time":"2099-01-01T05:00:00Z"}}]}}
]}}}}}}"#
    )
}

impl FakeAgy {
    pub fn install(dir: &Path) -> Self {
        std::fs::create_dir_all(dir).expect("mkdir");
        #[cfg(windows)]
        let (src_rel, name) = ("tests/fixtures/fake-agy.cmd", "agy.cmd");
        #[cfg(not(windows))]
        let (src_rel, name) = ("tests/fixtures/fake-agy.sh", "agy");
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join(src_rel);
        let dst = dir.join(name);
        std::fs::copy(&src, &dst).expect("copy fake agy");
        Self {
            dir: dir.to_path_buf(),
        }
    }

    pub fn bin(&self) -> PathBuf {
        #[cfg(windows)]
        {
            self.dir.join("agy.cmd")
        }
        #[cfg(not(windows))]
        {
            self.dir.join("agy")
        }
    }

    fn put(&self, name: &str, text: &str) {
        std::fs::write(self.dir.join(name), text).expect("write scenario file");
    }

    pub fn reset(&self) {
        for f in [
            "usage.out",
            "usage.err",
            "usage.code",
            "usage.sleep",
            "usage.spawn",
            "usage.child",
            "models.out",
            "models.err",
            "models.code",
            "models.sleep",
        ] {
            let _ = std::fs::remove_file(self.dir.join(f));
        }
    }

    pub fn ok(&self, five: f64, weekly: f64) -> &Self {
        self.reset();
        self.put("usage.out", &usage_json(five, weekly));
        self
    }

    pub fn fail(&self, stderr: &str, code: i32) -> &Self {
        self.reset();
        self.put("usage.err", stderr);
        self.put("usage.code", &code.to_string());
        self
    }

    pub fn stdout(&self, text: &str) -> &Self {
        self.reset();
        self.put("usage.out", text);
        self
    }

    pub fn hang(&self, secs: u32) -> &Self {
        self.reset();
        self.put("usage.sleep", &secs.to_string());
        self
    }

    /// Hang with a grandchild in the same process group.
    pub fn spawn_grandchild(&self) -> &Self {
        self.reset();
        self.put("usage.spawn", "1");
        self
    }

    /// The grandchild's pid, once it has started.
    pub fn grandchild_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.dir.join("usage.child"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// How many times `models` ran.
    pub fn model_calls(&self) -> usize {
        std::fs::read_to_string(self.dir.join("models.calls"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Steer a successful `models` run with the given TSV body.
    pub fn models(&self, tsv: &str) -> &Self {
        self.reset();
        self.put("models.out", tsv);
        self
    }

    /// Steer a failing `models` run.
    pub fn models_fail(&self, stderr: &str, code: i32) -> &Self {
        self.reset();
        self.put("models.err", stderr);
        self.put("models.code", &code.to_string());
        self
    }

    /// How many times `/usage` ran.
    pub fn calls(&self) -> usize {
        std::fs::read_to_string(self.dir.join("usage.calls"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }
}
