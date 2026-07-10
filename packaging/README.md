# Releasing and packaging cupel

Current state: `v0.1.2-beta` is live on GitHub Releases with all three
platform archives, and the `install.sh` one-liner resolves against it via
`releases/latest/download/`.

## Cutting a release

Pre-flight, in order:

1. CI is green on `main` (`.github/workflows/ci.yml` - fmt, clippy, tests
   on Ubuntu AND macOS; the Ubuntu leg is the only Linux coverage cupel
   gets before release).
2. Optionally bump the crate `version` fields in `crates/*/Cargo.toml` to
   match the tag. The binaries don't read it, but drift is confusing
   (tags are at 0.1.2-beta while the crates still say 0.1.0).

Then the tag IS the release (scheme in use: `vX.Y.Z-beta`):

```sh
git tag v0.1.3-beta && git push origin v0.1.3-beta
```

`.github/workflows/release.yml` builds the macOS universal binary and both
Linux musl binaries, and publishes a GitHub Release with the archives and a
`sha256sums.txt`. The release is created as a FULL release (not a
prerelease), which is what makes `releases/latest/download/` - and
therefore `install.sh` - point at it. Users install with:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

## Homebrew tap

One-time setup: create a GitHub repository named `homebrew-tap` and copy
`packaging/homebrew/cupel.rb` to `Formula/cupel.rb` in it. The formula in
this repo carries the real checksums for the current release, so the first
copy is publishable as-is.

Per release, bump `version` and refresh the three `sha256` values. The
checksums come straight from the release:

```sh
curl -fsSL https://github.com/rommeld/cupel/releases/latest/download/sha256sums.txt
```

Paste each hash into the matching `sha256` line, push the tap. Users
install with `brew install rommeld/tap/cupel`. (Automating this bump is
what `cargo-dist` or a small release-workflow step can do later.)

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
