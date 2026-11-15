## Summary

<!-- What changes, and why? Link the issue if there is one. -->

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor (no behavior change)
- [ ] Documentation
- [ ] Tests or CI
- [ ] Packaging or release

## How it was tested

<!-- Exact commands and results. Say what was NOT run, and why. -->

```text
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
```

## Behavior and compatibility

<!--
Call out changes to the public API, the on-disk format, credential handling,
the switch journal or exit codes. If this touches agy compatibility, say what
was observed and what remains unverified; do not upgrade a label without
evidence.
-->

## Safety checklist

- [ ] Tests and manual checks used temporary directories, synthetic
      `example.invalid` credentials and the fake `agy` script (or I explain why
      a check could not be done that way).
- [ ] I did not touch the real OS keyring, the real `$HOME`, `~/.gemini`, or
      the real `agy` executable.
- [ ] No credential, token, vault file, keyring payload, real email address or
      personal log is included in this pull request.
- [ ] The change does not add automatic account rotation, retry on quota
      exhaustion, or anything that bypasses Google's limits or a suspension.
- [ ] Security-relevant behavior was not weakened to make a check pass.
- [ ] User-facing documentation is updated when behavior changed, and
      `CHANGELOG.md` has an `Unreleased` entry.
- [ ] I agree to license this contribution under the MIT License.

## Remaining gaps

<!-- Known limitations, follow-up work, or checks that were NOT RUN. -->
