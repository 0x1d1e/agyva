//! End-to-end binary tests for v0.1.3 CLI commands: endpoints, bridge, env,
//! exec, profiles, statusline, checkpoints, route mutators, config reset,
//! doctor --fix, monitor --once --json, watch --json-stream, and completions.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use agyva::testing::credential;
use common::fake_agy::FakeAgy;
use tempfile::TempDir;

struct Sandbox {
    tmp: TempDir,
    fake: FakeAgy,
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn all(&self) -> String {
        format!("{}{}", self.out, self.err)
    }
}

impl Sandbox {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        for d in ["home", "agyhome", "work"] {
            std::fs::create_dir_all(tmp.path().join(d)).expect("mkdir");
        }
        let fake = FakeAgy::install(&tmp.path().join("fakeagy"));
        Self { tmp, fake }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn login(&self, sub: &str, email: &str, tag: &str) {
        std::fs::write(
            self.p("agyhome/antigravity-oauth-token"),
            credential(sub, email, tag),
        )
        .expect("login");
    }

    fn cmd_with(
        &self,
        args: &[&str],
        stdin_data: Option<&str>,
        cwd: Option<&std::path::Path>,
    ) -> Run {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agyva"));
        c.args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.p("home"))
            .env("AGYVA_CONFIG_DIR", self.p("cfg"))
            .env("AGYVA_STATE_DIR", self.p("state"))
            .env("AGYVA_CACHE_DIR", self.p("cache"))
            .env("AGYVA_AGY_HOME", self.p("agyhome"))
            .env("AGYVA_AGY_BIN", self.fake.bin())
            .env("AGYVA_BACKEND", "file")
            .env("AGYVA_TEST_MODE", "1")
            .env("AGYVA_UPDATE_CHECK", "0")
            .env("AGYVA_PASSPHRASE", "synthetic-e2e-passphrase")
            .env("NO_COLOR", "1");
        if let Some(dir) = cwd {
            c.current_dir(dir);
        }
        if stdin_data.is_some() {
            c.stdin(Stdio::piped());
        } else {
            c.stdin(Stdio::null());
        }
        c.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = c.spawn().expect("spawn agyva");
        if let Some(data) = stdin_data {
            let mut sin = child.stdin.take().expect("stdin");
            sin.write_all(data.as_bytes()).expect("write stdin");
        }
        let Output {
            status,
            stdout,
            stderr,
        } = child.wait_with_output().expect("wait agyva");
        Run {
            code: status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&stdout).into_owned(),
            err: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }

    fn run(&self, args: &[&str]) -> Run {
        self.cmd_with(args, None, None)
    }

    fn run_in(&self, dir: &std::path::Path, args: &[&str]) -> Run {
        self.cmd_with(args, None, Some(dir))
    }

    fn run_stdin(&self, args: &[&str], stdin_data: &str) -> Run {
        self.cmd_with(args, Some(stdin_data), None)
    }
}

struct MockHttpServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockHttpServer {
    fn start(expected_auth: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("local_addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                        let mut request_line = String::new();
                        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                            continue;
                        }
                        let mut auth_ok = false;
                        loop {
                            let mut h = String::new();
                            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                                break;
                            }
                            if h == "\r\n" || h == "\n" {
                                break;
                            }
                            if let Some((k, v)) = h.split_once(':')
                                && k.trim().eq_ignore_ascii_case("authorization")
                                && v.trim() == expected_auth
                            {
                                auth_ok = true;
                            }
                        }

                        let (status, body) = if !auth_ok {
                            (
                                "401 Unauthorized",
                                r#"{"error":{"message":"unauthorized"}}"#,
                            )
                        } else if request_line.starts_with("GET /v1/models") {
                            (
                                "200 OK",
                                r#"{"object":"list","data":[{"id":"mock-model-1","owned_by":"mock-org"},{"id":"mock-model-2","owned_by":"mock-org"}]}"#,
                            )
                        } else {
                            (
                                "200 OK",
                                r#"{"id":"chatcmpl-1","object":"chat.completion","model":"mock-model-1","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
                            )
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{addr}/v1"),
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[test]
fn endpoint_crud_presets_mock_http_and_unified_use_roundtrip() {
    let sb = Sandbox::new();
    sb.login("sub-alice", "alice@example.invalid", "alice");
    assert_eq!(sb.run(&["add", "--alias", "alice"]).code, 0);

    // Presets
    let presets = sb.run(&["ep", "presets", "--json"]);
    assert_eq!(presets.code, 0, "{}", presets.all());
    let pv: serde_json::Value = serde_json::from_str(&presets.out).expect("presets json");
    assert_eq!(pv["presets"].as_array().map(Vec::len), Some(17));

    // Mock HTTP server
    let server = MockHttpServer::start("Bearer sk-secret-ep-123");
    let add = sb.run_stdin(
        &[
            "endpoint",
            "add",
            "myep",
            "--base-url",
            &server.base_url,
            "--protocol",
            "openai",
            "--model",
            "mock-model-1",
            "--api-key-stdin",
            "--json",
        ],
        "sk-secret-ep-123\n",
    );
    assert_eq!(add.code, 0, "{}", add.all());
    let add_json: serde_json::Value = serde_json::from_str(&add.out).expect("add json");
    assert_eq!(add_json["endpoint"]["alias"], "myep");
    assert_eq!(add_json["probe"]["reachable"], true);
    assert_eq!(add_json["probe"]["model_count"], 2);
    assert!(!add.out.contains("sk-secret-ep-123"));

    // list --json and show --json never leak secrets
    let list = sb.run(&["endpoint", "list", "--json"]);
    assert_eq!(list.code, 0, "{}", list.all());
    assert!(!list.out.contains("sk-secret-ep-123"));
    let lv: serde_json::Value = serde_json::from_str(&list.out).expect("list json");
    assert_eq!(lv["endpoints"][0]["alias"], "myep");
    assert_eq!(lv["endpoints"][0]["model_count"], 2);

    let show = sb.run(&["ep", "show", "myep", "--json"]);
    assert_eq!(show.code, 0, "{}", show.all());
    assert!(!show.out.contains("sk-secret-ep-123"));
    let sv: serde_json::Value = serde_json::from_str(&show.out).expect("show json");
    assert_eq!(sv["default_model"], "mock-model-1");
    assert_eq!(sv["models"][0]["id"], "mock-model-1");

    // test & models --refresh
    let test_res = sb.run(&["ep", "test", "myep", "--json"]);
    assert_eq!(test_res.code, 0, "{}", test_res.all());
    let tv: serde_json::Value = serde_json::from_str(&test_res.out).expect("test json");
    assert_eq!(tv["reachable"], true);
    assert_eq!(tv["model_count"], 2);

    let models_res = sb.run(&["ep", "models", "myep", "--refresh", "--json"]);
    assert_eq!(models_res.code, 0, "{}", models_res.all());
    let mv: serde_json::Value = serde_json::from_str(&models_res.out).expect("models json");
    assert_eq!(mv["refreshed"], true);
    assert_eq!(mv["model_count"], 2);

    // update + rename + unified use ep:... + OAuth switch roundtrip
    let upd = sb.run(&[
        "ep",
        "update",
        "myep",
        "--model",
        "mock-model-2",
        "--header",
        "X-Test=1",
        "--json",
    ]);
    assert_eq!(upd.code, 0, "{}", upd.all());

    let ren = sb.run(&["ep", "rename", "myep", "workep", "--json"]);
    assert_eq!(ren.code, 0, "{}", ren.all());

    // Unified `use ep:workep`
    let u_ep = sb.run(&["use", "ep:workep"]);
    assert_eq!(u_ep.code, 0, "{}", u_ep.all());
    let settings = std::fs::read_to_string(sb.p("agyhome/settings.json")).expect("settings");
    assert!(settings.contains("\"modelProvider\": \"custom\""));
    assert!(settings.contains("workep"));
    assert!(!settings.contains("sk-secret-ep-123"));

    // Unified `switch alice` restores OAuth and clears active endpoint
    let u_oauth = sb.run(&["switch", "alice"]);
    assert_eq!(u_oauth.code, 0, "{}", u_oauth.all());
    let settings_after = std::fs::read_to_string(sb.p("agyhome/settings.json")).expect("settings");
    assert!(!settings_after.contains("customModelsConfig"));

    let ep_list_after = sb.run(&["ep", "list", "--json"]);
    let elv: serde_json::Value = serde_json::from_str(&ep_list_after.out).expect("json");
    assert_eq!(elv["endpoints"][0]["active"], false);

    // Remove endpoint
    assert_eq!(sb.run(&["ep", "remove", "workep"]).code, 2);
    assert_eq!(sb.run(&["ep", "remove", "workep", "--yes"]).code, 0);
}

#[test]
fn env_and_exec_inject_secrets_safely_and_manage_ephemeral_bridge() {
    let sb = Sandbox::new();
    let add = sb.run_stdin(
        &[
            "ep",
            "add",
            "localai",
            "--base-url",
            "http://127.0.0.1:18080/v1",
            "--protocol",
            "openai",
            "--model",
            "qwen-3",
            "--api-key-stdin",
            "--no-fetch",
        ],
        "sk-exec-secret-val\n",
    );
    assert_eq!(add.code, 0, "{}", add.all());

    // env formats
    let bash_env = sb.run(&["env", "--endpoint", "localai", "--shell", "bash"]);
    assert_eq!(bash_env.code, 0, "{}", bash_env.all());
    assert!(
        bash_env
            .out
            .contains("export OPENAI_BASE_URL='http://127.0.0.1:18080/v1'")
    );
    assert!(bash_env.out.contains("export AGYVA_MODEL='qwen-3'"));

    let fish_env = sb.run(&["env", "--endpoint", "localai", "--shell", "fish"]);
    assert_eq!(fish_env.code, 0, "{}", fish_env.all());
    assert!(
        fish_env
            .out
            .contains("set -gx OPENAI_BASE_URL 'http://127.0.0.1:18080/v1'")
    );

    let ps_env = sb.run(&["env", "--endpoint", "localai", "--shell", "powershell"]);
    assert_eq!(ps_env.code, 0, "{}", ps_env.all());
    assert!(
        ps_env
            .out
            .contains("$env:OPENAI_BASE_URL = 'http://127.0.0.1:18080/v1'")
    );

    let dotenv = sb.run(&["env", "--endpoint", "localai", "--shell", "dotenv"]);
    assert_eq!(dotenv.code, 0, "{}", dotenv.all());
    assert!(
        dotenv
            .out
            .contains("OPENAI_BASE_URL=\"http://127.0.0.1:18080/v1\"")
    );

    // exec injects env vars directly into child without printing secrets to stdout
    let exec_ok = sb.run(&[
        "exec",
        "--endpoint",
        "localai",
        "--model",
        "override-m",
        "--",
        "/bin/sh",
        "-c",
        "test \"$OPENAI_API_KEY\" = \"sk-exec-secret-val\" && echo \"$AGYVA_ENDPOINT:$AGYVA_MODEL\"",
    ]);
    assert_eq!(exec_ok.code, 0, "{}", exec_ok.all());
    assert_eq!(exec_ok.out.trim(), "localai:override-m");
    assert!(!exec_ok.out.contains("sk-exec-secret-val"));
    assert!(!exec_ok.err.contains("sk-exec-secret-val"));

    // exec --bridge starts ephemeral 127.0.0.1:0 bridge and stops on exit
    let exec_bridge = sb.run(&[
        "exec",
        "--endpoint",
        "localai",
        "--bridge",
        "--",
        "/bin/sh",
        "-c",
        "test -n \"$AGYVA_BRIDGE_URL\" && echo \"$AGYVA_BRIDGE_URL\"",
    ]);
    assert_eq!(exec_bridge.code, 0, "{}", exec_bridge.all());
    assert!(exec_bridge.out.trim().starts_with("http://127.0.0.1:"));

    // Child exit code propagation
    let exec_fail = sb.run(&[
        "exec",
        "--endpoint",
        "localai",
        "--",
        "/bin/sh",
        "-c",
        "exit 42",
    ]);
    assert_eq!(exec_fail.code, 42);
}

#[test]
fn profiles_statusline_checkpoints_route_config_doctor_monitor_and_completions() {
    let sb = Sandbox::new();
    sb.login("sub-alice", "alice@example.invalid", "alice");
    assert_eq!(sb.run(&["add", "--alias", "alice"]).code, 0);
    sb.login("sub-bob", "bob@example.invalid", "bob");
    assert_eq!(sb.run(&["add", "--alias", "bob"]).code, 0);
    sb.fake.ok(0.65, 0.35);
    assert_eq!(sb.run(&["usage"]).code, 0);

    // Local profile init, which, set, and use
    let work_dir = sb.p("work");
    let p_init = sb.run_in(
        &work_dir,
        &[
            "prof",
            "init",
            "--local",
            "--account",
            "alice",
            "--model",
            "gemini-2.5-pro",
            "--warn-below",
            "40",
            "--critical-below",
            "15",
            "--json",
        ],
    );
    assert_eq!(p_init.code, 0, "{}", p_init.all());
    assert!(work_dir.join(".agyva.toml").exists());

    let p_which = sb.run_in(&work_dir, &["profile", "which"]);
    assert_eq!(p_which.code, 0, "{}", p_which.all());
    assert!(p_which.out.contains("local"), "{}", p_which.out);

    let p_set = sb.run_in(&work_dir, &["profile", "set", "model", "claude-sonnet-4"]);
    assert_eq!(p_set.code, 0, "{}", p_set.all());

    let p_use = sb.run_in(&work_dir, &["profile", "use"]);
    assert_eq!(p_use.code, 0, "{}", p_use.all());

    // Global profile create, list, show, delete
    let p_create = sb.run(&[
        "profile",
        "create",
        "team",
        "--account",
        "bob",
        "--warn-below",
        "30",
        "--json",
    ]);
    assert_eq!(p_create.code, 0, "{}", p_create.all());
    assert_eq!(sb.run(&["profile", "list", "--json"]).code, 0);
    assert_eq!(sb.run(&["profile", "show", "team", "--json"]).code, 0);
    assert_eq!(sb.run(&["profile", "delete", "team", "--yes"]).code, 0);

    // Statusline formats
    for fmt in ["compact", "plain", "starship", "tmux", "waybar", "json"] {
        let sl = sb.run_in(&work_dir, &["statusline", "--format", fmt]);
        assert_eq!(sl.code, 0, "{fmt}: {}", sl.all());
        assert!(!sl.out.trim().is_empty(), "{fmt}");
        if fmt == "waybar" {
            let w: serde_json::Value = serde_json::from_str(&sl.out).expect("waybar json");
            assert!(w["text"].is_string() && w["tooltip"].is_string() && w["class"].is_string());
        } else if fmt == "json" {
            let j: serde_json::Value = serde_json::from_str(&sl.out).expect("statusline json");
            assert_eq!(j["schema_version"], 1);
            assert_eq!(j["active_account"], "alice");
        } else if fmt == "tmux" {
            assert!(sl.out.contains("#[fg="), "{}", sl.out);
        }
    }

    // Checkpoints list, restore, prune
    sb.login("sub-carol", "carol@example.invalid", "carol");
    assert_eq!(sb.run(&["use", "bob", "--force"]).code, 0);
    let ckpt_list = sb.run(&["ckpt", "list", "--json"]);
    assert_eq!(ckpt_list.code, 0, "{}", ckpt_list.all());
    let ckpts: serde_json::Value = serde_json::from_str(&ckpt_list.out).expect("ckpt json");
    let first_ckpt = ckpts["checkpoints"][0]["name"]
        .as_str()
        .expect("checkpoint name")
        .to_owned();
    let ckpt_restore = sb.run(&[
        "checkpoints",
        "restore",
        &first_ckpt,
        "--force",
        "--yes",
        "--json",
    ]);
    assert_eq!(ckpt_restore.code, 0, "{}", ckpt_restore.all());
    let ckpt_prune = sb.run(&["ckpt", "prune", "--include-pinned", "--yes", "--json"]);
    assert_eq!(ckpt_prune.code, 0, "{}", ckpt_prune.all());

    // Route init / add / remove
    let r_init = sb.run(&["route", "init", "--yes", "--json"]);
    assert_eq!(r_init.code, 0, "{}", r_init.all());
    let r_add = sb.run(&[
        "route",
        "add",
        "work",
        "--accounts",
        "alice,bob",
        "--ranking",
        "least-used",
        "--auto-switch",
        "--json",
    ]);
    assert_eq!(r_add.code, 0, "{}", r_add.all());
    let r_rem = sb.run(&["route", "remove", "work", "--yes", "--json"]);
    assert_eq!(r_rem.code, 0, "{}", r_rem.all());

    // Config reset
    assert_eq!(sb.run(&["config", "set", "color", "never"]).code, 0);
    let rst = sb.run(&["config", "reset", "color"]);
    assert_eq!(rst.code, 0, "{}", rst.all());
    assert_eq!(rst.out.trim(), "color = auto");

    // Doctor --fix
    std::fs::set_permissions(sb.p("cfg"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let stale_lock = sb.p("cache/quota.lock");
    std::fs::write(&stale_lock, "999999999").unwrap();
    let fix = sb.run(&["doctor", "--fix", "--json"]);
    assert_eq!(fix.code, 0, "{}", fix.all());
    let fv: serde_json::Value = serde_json::from_str(&fix.out).expect("doctor fix json");
    assert!(!fv["actions"].as_array().unwrap().is_empty());
    assert!(!stale_lock.exists());
    assert_eq!(
        std::fs::metadata(sb.p("cfg")).unwrap().permissions().mode() & 0o777,
        0o700
    );

    // Monitor --once --json
    let mon = sb.run(&["mon", "--once", "--all", "--json"]);
    assert_eq!(mon.code, 0, "{}", mon.all());
    let mv: serde_json::Value = serde_json::from_str(&mon.out).expect("monitor json");
    assert_eq!(mv["schema_version"], 1);
    assert!(mv["rows"].as_array().is_some());

    // Watch --once --json-stream
    let watch_stream = sb.run(&["watch", "--once", "--warn-below", "90", "--json-stream"]);
    assert_eq!(watch_stream.code, 0, "{}", watch_stream.all());
    for line in watch_stream.out.lines() {
        let ev: serde_json::Value = serde_json::from_str(line).expect("ndjson watch event");
        assert!(ev["event"].is_string(), "{ev}");
    }

    // Completions powershell / elvish + fish dynamic endpoint/profile completions
    for shell in ["powershell", "elvish"] {
        let comp = sb.run(&["completions", shell]);
        assert_eq!(comp.code, 0, "{shell}: {}", comp.all());
        assert!(comp.out.contains("agyva"), "{shell}");
    }
    let fish_comp = sb.run(&["completions", "fish"]);
    assert_eq!(fish_comp.code, 0);
    assert!(fish_comp.out.contains("__agyva_endpoints"));
    assert!(fish_comp.out.contains("__agyva_profiles"));
    assert!(fish_comp.out.contains("__agyva_checkpoints"));

    // Command aliases and -q
    assert_eq!(sb.run(&["-q", "ls", "--json"]).code, 0);
    assert_eq!(sb.run(&["st", "--json"]).code, 0);
}

#[test]
fn bridge_command_prints_exports_and_stops_on_sigint() {
    use std::io::Read as _;
    use std::time::{Duration, Instant};

    let sb = Sandbox::new();
    let add = sb.run_stdin(
        &[
            "ep",
            "add",
            "bep",
            "--base-url",
            "http://127.0.0.1:18081/v1",
            "--protocol",
            "openai",
            "--api-key-stdin",
            "--no-fetch",
        ],
        "sk-bridge-secret\n",
    );
    assert_eq!(add.code, 0, "{}", add.all());

    let mut child = Command::new(env!("CARGO_BIN_EXE_agyva"))
        .args(["bridge", "--endpoint", "bep"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", sb.p("home"))
        .env("AGYVA_CONFIG_DIR", sb.p("cfg"))
        .env("AGYVA_STATE_DIR", sb.p("state"))
        .env("AGYVA_CACHE_DIR", sb.p("cache"))
        .env("AGYVA_AGY_HOME", sb.p("agyhome"))
        .env("AGYVA_AGY_BIN", sb.fake.bin())
        .env("AGYVA_BACKEND", "file")
        .env("AGYVA_TEST_MODE", "1")
        .env("AGYVA_UPDATE_CHECK", "0")
        .env("AGYVA_PASSPHRASE", "synthetic-e2e-passphrase")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    let mut stdout = child.stdout.take().expect("stdout");
    let mut buf = [0u8; 512];
    let mut captured = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !captured.contains("ANTHROPIC_BASE_URL") {
        let n = stdout.read(&mut buf).expect("read bridge banner");
        if n == 0 {
            break;
        }
        captured.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(
        captured.contains("Bridge listening on http://127.0.0.1:")
            && captured.contains("GOOGLE_GEMINI_BASE_URL=")
            && captured.contains("OPENAI_BASE_URL=")
            && captured.contains("ANTHROPIC_BASE_URL="),
        "unexpected bridge output: {captured}"
    );
    assert!(!captured.contains("sk-bridge-secret"));

    let st = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(st.success());
    let wait_deadline = Instant::now() + Duration::from_secs(5);
    let code = loop {
        if let Some(status) = child.try_wait().expect("poll") {
            break status.code();
        }
        if Instant::now() > wait_deadline {
            let _ = child.kill();
            panic!("bridge ignored SIGINT");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(code, Some(0));
}

#[test]
fn env_refuses_profile_keys_with_shell_metacharacters() {
    for bad_key in ["FOO$(id)", "FOO;id", "FOO`id`", "1BAD", "FOO-BAR"] {
        let sb = Sandbox::new();
        let proj = sb.p("work");
        std::fs::write(
            proj.join(".agyva.toml"),
            format!("[env]\n\"{bad_key}\" = \"val\"\n"),
        )
        .expect("write .agyva.toml");

        for shell in ["bash", "fish", "powershell"] {
            let res = sb.run_in(&proj, &["env", "--shell", shell]);
            assert_eq!(
                res.code,
                2,
                "expected usage error for key `{bad_key}` on {shell}: {}",
                res.all()
            );
            assert!(
                res.out.is_empty(),
                "stdout must stay empty on error for `{bad_key}`: {}",
                res.out
            );
            assert!(
                res.err.contains("invalid environment key"),
                "expected invalid environment key message for `{bad_key}`: {}",
                res.err
            );
        }
    }
}
