# Contributing to agyva

Contributions must keep `agyva` predictable, auditable, and honest about what is verified. By contributing, you agree to the [Code of Conduct](#code-of-conduct) and license your work under the [MIT License](LICENSE).

## Rules

- **Never test with real accounts.** Do not point tests at the real OS keyring, `$HOME`, `~/.gemini`, or the real `agy` binary. Use temp directories, `example.invalid` data, synthetic tokens, and the fake `agy` script. See [safety rules for tests](docs/development.md#safety-rules-for-tests).
- **No automatic account rotation.** Switching is manual unless explicitly opted into via `routing.toml` + `auto_switch_ack`. Changes that add background rotation, quota-error retries, or limit bypasses will be rejected.
- **Never weaken security checks to pass tests.** Fix the behavior or the broken test assertion directly.
- **Report security issues privately.** Follow [`SECURITY.md`](SECURITY.md); never post real tokens, credentials, or vault files in issues or PRs.

## Build and test

Requires Rust stable (minimum 1.88, see `rust-toolchain.toml`) and `tmux` (for `tests/pty_smoke.rs`). A Secret Service daemon is not needed for the default suite.

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
python3 -I scripts/check-links.py
```

`make ci` runs all of the checks above. `cargo deny check` (configured in [`deny.toml`](deny.toml)) requires `cargo-deny` and network access.

The opt-in test `tests/real_keyring.rs` touches the real Secret Service daemon using a throwaway item only; do not modify it to touch `gemini`, `antigravity`, or `agyva`:

```sh
AGYVA_REAL_KEYRING_TEST=1 cargo test --test real_keyring -- --ignored
```

## Workflow and conventions

1. Open an issue before starting non-trivial changes; small doc or bug fixes can go straight to a PR.
2. Keep diffs focused; avoid unrelated formatting changes.
3. Follow `src/` conventions:
   - `#![forbid(unsafe_code)]` crate-wide.
   - No `unwrap()`, `expect()`, `panic!`, or `unreachable!` in non-test code.
   - Return `Error` with an `ErrorKind` (mapping to exit code) and an actionable `hint` where useful.
   - Read environment variables once through `Env`, not ad hoc.
4. Add regression tests through the library or sandboxed binary for changed behavior.
5. Update affected docs (`README.md`, `docs/*.md`, `CHANGELOG.md` under `Unreleased`). Use the evidence labels in [`docs/compatibility.md`](docs/compatibility.md); never claim unverified `agy` compatibility.
6. Write imperative commit subjects (for example, `Fix selector exit code`). Do not commit `target/`, `dist/`, logs, secrets, or real account captures.

## Screenshots and captures

Keep README and dashboard captures free of real identities by regenerating from the sanitized replay or synthetic data:

```sh
scripts/capture-tui.sh                      # updates docs/tui-capture.txt, docs/tui.svg, docs/tui-usage.svg
scripts/capture-tui.sh --synthetic /tmp/demo.txt
```

See [capture provenance](docs/dashboard.md#capture-provenance) for what each capture preserves.

## Code of Conduct

Be respectful and constructive. Harassment, discrimination, and sharing others' private data are unacceptable. Maintainers may remove offending content or block repeat offenders. Report issues privately via GitHub's security advisory form.

## License

MIT. See [LICENSE](LICENSE).
