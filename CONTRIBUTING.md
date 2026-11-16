# Contributing to agyva

Thanks for taking a look. Agyva is a small, security-sensitive Rust tool:
contributions that keep it predictable, readable and honest about what it does
are welcome.

By participating you agree to follow the [Code of Conduct](#code-of-conduct)
and to license your contribution under the [MIT License](LICENSE).

## Before you start

- **Never test with your real accounts.** Do not run the test suite against the
  real OS keyring, the real `$HOME`, `~/.gemini`, or the real `agy` executable.
  Tests use temporary directories, obviously fake data (`example.invalid`,
  synthetic tokens) and a fake `agy` script. See
  [`docs/development.md`](docs/development.md#safety-rules-for-tests).
- **No automatic account rotation.** Switching is manual by design; a change
  that adds background rotation, quota-error retries or a bypass of Google's
  limits will not be accepted.
- **Do not weaken security checks to make a test pass.** If a test is wrong,
  explain why and fix the test with the behavior.
- Report security problems privately, per [`SECURITY.md`](SECURITY.md). Do not
  open a public issue containing credentials, tokens or personal data.

## Build and test

Requirements: Linux, Rust stable (minimum 1.88; `rustup` reads
`rust-toolchain.toml`), and the native dependencies used by CI (`tmux` for the
pty smoke test; a Secret Service provider is not needed for tests).

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

`cargo deny check` (see [`deny.toml`](deny.toml)) needs `cargo-deny` and network
access for the advisory database. The same checks run in CI; `make ci` runs the
local set.

The opt-in test in `tests/real_keyring.rs` touches the real Secret Service
daemon and is **not** run by default. It uses only a throwaway item; do not
change it to touch `gemini`, `antigravity` or `agyva`:

```sh
AGYVA_REAL_KEYRING_TEST=1 cargo test --test real_keyring -- --ignored
```

## Working on a change

1. Open an issue for anything non-trivial first, so the approach can be agreed
   before you spend time on it. Small documentation fixes and obvious bug fixes
   can go straight to a pull request.
2. Keep the diff focused. Unrelated reformatting makes review harder and will
   usually be asked to be split out.
3. Follow the existing conventions under `src/`:
   - no `unsafe`;
   - non-test code has no `unwrap`, `expect`, `panic!` or `unreachable!`;
   - errors carry a kind (which decides the exit code) and, when there is a
     concrete next step, a hint;
   - read the process environment once, through `Env`, not ad hoc.
4. Add a check for behavior that could regress. Prefer an observable test
   through the library or the real binary in a sandbox over a test per
   function. New behavior that cannot be tested in the sandbox needs a note in
   the pull request explaining why.
5. Update the docs that state the affected behavior (`README.md` and/or
   `docs/*.md`), plus `CHANGELOG.md` under `Unreleased`. Do not claim that
   something was verified against a real `agy` unless it was; use the labels in
   [`docs/compatibility.md`](docs/compatibility.md).
6. Run the checks above before pushing. If a check fails for a reason you
   believe is unrelated, say so in the pull request instead of removing it.

## Commits and pull requests

- Write commit subjects in the imperative mood ("Fix selector exit code", not
  "Fixed ..."). Keep each commit coherent; there is no required commit message
  format, DCO or CLA.
- In the pull request, say what changed, why, how it was tested, and any
  behavior or compatibility that remains unverified. Fill in the template's
  safety checklist.
- Public API, on-disk format, credential handling and switch/recovery changes
  get a closer review. Call them out explicitly.
- Do not include generated `target/` files, `dist/` archives, screenshots of
  real accounts, logs with real data, or secrets of any kind.

## Documentation and screenshots

Documentation is part of the product. Keep it accurate about what is
implemented, what is observed and what is unverified, and update the docs that
state the behavior you changed.

The README and dashboard screenshots must stay free of real identities:
regenerate them from the sanitized replay, or with explicitly synthetic data,
never from a real account registry:

```sh
scripts/capture-tui.sh                      # docs/tui-capture.txt, docs/tui.svg, docs/tui-usage.svg
scripts/capture-tui.sh --synthetic /tmp/demo.txt
```

The default replay reads [sanitized evidence](docs/capture-data/accounts.json)
into scratch directories and renders the production views; `--synthetic` labels
its output as made up. [Capture provenance](docs/dashboard.md#capture-provenance)
records what each capture preserves. Screenshots containing real accounts will
be rejected.

## Code of Conduct

Be respectful and constructive. Harassment, discrimination and disclosure of
another person's private information are not acceptable. Maintainers may edit
or remove contributions and comments that do not meet this standard, and may
block repeat offenders. Report concerns privately to the maintainers through
the repository's security advisory form if you do not want to raise them in a
thread.

## License

Agyva is MIT licensed. By submitting a contribution you agree that it is your
work, or that you have the right to submit it, and that it may be distributed
under the same [MIT License](LICENSE).
