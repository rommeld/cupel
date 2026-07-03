# Releasing and packaging cupel

## Cutting a release

```sh
git tag v0.1.0 && git push origin v0.1.0
```

That's the whole trigger: `.github/workflows/release.yml` builds the macOS
universal binary and both Linux musl binaries, and publishes a GitHub
Release with the archives and a `sha256sums.txt`. Users install with:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

## Homebrew tap

One-time setup: create a GitHub repository named `homebrew-tap` and copy
`packaging/homebrew/cupel.rb` to `Formula/cupel.rb` in it.

Per release, update the version and checksums. The checksums come straight
from the release:

```sh
curl -fsSL https://github.com/rommeld/cupel/releases/download/v0.1.0/sha256sums.txt
```

Paste each hash into the matching `sha256` line, bump `version`, push the
tap. Users install with `brew install rommeld/tap/cupel`. (Automating this
bump is what `cargo-dist` or a small release-workflow step can do later.)

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
| Linux x86_64 / aarch64 | static musl binaries, any distro |
| Windows | not yet: the bash tool is Unix-only (process groups, `kill`) |
