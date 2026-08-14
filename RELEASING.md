# Releasing rsbts

Releases are built from an annotated `vX.Y.Z` tag by the pinned GitHub Actions
workflow. Do not publish from an uncommitted workstation tree.

1. Confirm the version in `Cargo.toml` and `Cargo.lock`, move release notes from
   `Unreleased` into the dated changelog section, and review the public API
   report against the previous tag.
2. Run the complete local validation:

   ```bash
   cargo fmt --all -- --check
   cargo clippy --locked --all-targets --all-features -- -D warnings
   cargo test --locked --all-targets
   RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
   cargo deny --locked check
   cargo semver-checks --baseline-rev v0.2.0
   cargo package --locked
   cargo check --locked --manifest-path fuzz/Cargo.toml --bins
   cargo check --locked --manifest-path benchmarks/Cargo.toml --release
   cargo run --locked --manifest-path benchmarks/Cargo.toml --release --quiet
   ```

3. Confirm the pull request CI, including minimum Rust, Linux/macOS/Windows,
   coverage, selected mutation, fuzz-smoke, and million-track jobs, is green.
4. Commit the final version and changelog, create an annotated tag, and push it:

   ```bash
   git tag -a v0.4.0 -m "rsbts 0.4.0"
   git push origin v0.4.0
   ```

5. The release workflow builds four archives, generates CycloneDX JSON,
   SHA-256 checksums, and GitHub build-provenance attestations, then creates the
   GitHub release. Download every artifact, verify `SHA256SUMS` and its
   attestation, and smoke-test one archive before announcing the release.

Re-running a failed workflow must use the same immutable tag and source commit.
Never move a published tag or replace an existing release asset.
