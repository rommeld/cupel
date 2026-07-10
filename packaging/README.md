# Releasing and packaging cupel

Current state: `v0.1.3-beta` (adds `@path` file references in the TUI) is
live on GitHub Releases with all three platform archives, and the
`install.sh` one-liner resolves against it via `releases/latest/download/`.

## Cutting a release

Pre-flight, in order:

1. CI is green on `main` (`.github/workflows/ci.yml` - fmt, clippy, tests
   on Ubuntu AND macOS; the Ubuntu leg is the only Linux coverage cupel
   gets before release).
2. Optionally bump the crate `version` fields in `crates/*/Cargo.toml` to
   match the tag. The binaries don't read it, but drift is confusing
   (tags are at 0.1.3-beta while the crates still say 0.1.0).

Then the tag IS the release (scheme in use: `vX.Y.Z-beta`):

```sh
git tag v0.1.4-beta && git push origin v0.1.4-beta
```

`.github/workflows/release.yml` builds the macOS universal binary and both
Linux musl binaries, and publishes a GitHub Release with the archives and a
`sha256sums.txt`. The release is created as a FULL release (not a
prerelease), which is what makes `releases/latest/download/` - and
therefore `install.sh` - point at it. Users install with:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

## Homebrew tap (automated)

The formula bump is AUTOMATED: after every release, the `homebrew` job in
`release.yml` regenerates `cupel.rb` (via `packaging/update-formula.sh`)
with the new version + checksums and pushes it to the tap repository.
Users install with `brew install rommeld/tap/cupel`.

One-time setup (until then the job skips with a notice, never failing a
release):

1. Create a GitHub repository named `homebrew-tap`; copy
   `packaging/homebrew/cupel.rb` to `Formula/cupel.rb` in it (the formula
   here carries the real checksums for the current release, so the first
   copy is publishable as-is).
2. Create a fine-grained personal access token with `Contents:
   Read and write` permission on ONLY the `homebrew-tap` repository. The
   workflow's default `GITHUB_TOKEN` cannot push to other repos - that's
   why a dedicated token is needed.
3. Add it to the `cupel` repository as an Actions secret named
   `HOMEBREW_TAP_TOKEN`.

Manual fallback (or local dry-run) for any release:

```sh
curl -fsSL https://github.com/rommeld/cupel/releases/latest/download/sha256sums.txt -o /tmp/sums.txt
packaging/update-formula.sh 0.1.4-beta /tmp/sums.txt
# then copy packaging/homebrew/cupel.rb into the tap's Formula/ and push
```

## macOS signing and notarization (optional)

Unsigned binaries downloaded via a BROWSER are blocked by Gatekeeper until
`xattr -d com.apple.quarantine cupel`. Installs via `install.sh`, `brew`, or
any curl-based flow are unaffected (no quarantine attribute is set).

To remove the browser-download friction entirely: join the Apple Developer
Program ($99/year), then sign and notarize in the release workflow with
`codesign` + `notarytool` using a `Developer ID Application` certificate
stored in repository secrets.

## crates.io (for users who DO have Rust)

`cargo publish -p cupel-core`, then `-p cupel-agent`, then
`-p cupel-coding-agent` (dependency order matters; path dependencies need
`version =` fields added first). After that, Rust users can
`cargo install cupel-coding-agent`. Optional - the binary channels above
serve everyone else.

## cargo-dist migration

This pipeline is hand-rolled so every step is visible. `cargo-dist`
(`dist init`) can generate an equivalent workflow plus installers and the
Homebrew formula from Cargo.toml metadata, and keeps them updated with the
tool. Migrate when maintaining the YAML by hand stops being instructive:
delete `release.yml` + `install.sh`, run `dist init`, commit its output.

## Platform support

| Platform | Status |
| --- | --- |
| macOS (Apple Silicon + Intel) | universal binary |
| Linux x86_64 / aarch64 | static musl binaries, any distro (kill/timeout handling verified by CI since v0.1.1-beta) |
| Windows | not yet: the bash tool is Unix-only (process groups, `kill`) |
