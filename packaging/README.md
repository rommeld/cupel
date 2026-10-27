# Releasing and packaging cupel

Each release has three platform archives. Starting with the ACP packaging
release, each contains `cupel`, `cupel-acp`, `LICENSE`, `README.md`, and ACP client
examples under `examples/`. Archive names and the published `sha256sums.txt`
stay unchanged, so existing Cupel installers and `/update` remain compatible.

## Cutting a release

Pre-flight, in order:

1. CI is green on `main` (`.github/workflows/ci.yml` - fmt, strict Clippy, Rust
   tests, and packaging tests on Ubuntu and macOS). The release workflow runs
   the same checks on the tagged commit before publication. Rust and Clippy use
   `rust-toolchain.toml`.
2. Choose a `vX.Y.Z` or `vX.Y.Z-prerelease` tag. The release workflow sets the
   inherited workspace version from it before building both executables.

Then the tag IS the release (scheme in use: `vX.Y.Z-beta`):

```sh
git tag v0.1.4-beta && git push origin v0.1.4-beta
```

`.github/workflows/release.yml` builds both executables as macOS universal
binaries and Linux musl binaries, and publishes a GitHub Release with the archives and a
`sha256sums.txt`. The release is created as a FULL release (not a
prerelease), which is what makes `releases/latest/download/` - and
therefore `install.sh` - point at it. Users install with:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

Release publication is retryable: an interrupted draft resumes its uploads.
The same publish job then commits `CHANGELOG.md` and the workspace version onto
`main` and updates Homebrew - no PR or auto-merge. It checks out `main` itself
(a tag push would otherwise check out the tag) with full history and tags, and
the push authenticates with the persisted `GITHUB_TOKEN`.

For that push to clear `main`'s ruleset, the **GitHub Actions** app must be a
bypass actor on it. Add it once under **Settings > Rules > main > Bypass list**
(or with `PUT /repos/{owner}/{repo}/rulesets/{id}`): actor type *Integration*,
id `15368`, mode *Always*. Keep the pull-request and required-check rules in
place for human contributors; the job only needs `contents: write` and never
approves or merges anything. Because it pushes without a PR, a concurrent push
to `main` makes it fail (never force-pushes) - re-run the job.

Publishing is serialized and both updates compare SemVer precedence against
their current version. Older tags still get a GitHub Release, but do not replace
newer workspace versions, changelog sections, or formulas.

Re-running the failed `publish` job from its run records the release. For a tag
older than this workflow, run the helper in a clean checkout of `main` with full
history and tags and push the result through a PR (the bypass only covers the
Actions app):

```sh
sh packaging/update-release-metadata.sh v0.33.8
```

Re-running an old release run uses that tag's original workflow definition.
It does not pick up workflow fixes merged later.

Both executables go into `${CUPEL_INSTALL_DIR:-${CUPEL_HOME:-$HOME/.cupel}/bin}`.
The installer preserves compatibility with older `cupel`-only archives and
warns that ACP needs a newer release. Re-running it updates both executables;
the TUI's `/update` still updates only `cupel`.

### Artifact verification

Before upload, every release runner verifies the actual archive's checksum,
both executable files and their `--help`, installation into a clean default and
custom home/bin directory without Rust, and an ACP initialize/session/prompt
exchange using loopback fake inference. macOS also checks both universal slices.
All three build/run checks must pass before the release can be published.

To repeat the checks locally on a matching host (Python 3.12+, no Python packages):

```sh
python3 -m unittest discover -s packaging -p 'test_*.py' -v
python3 packaging/check-release.py /path/to/cupel-<platform>.tar.gz /path/to/sha256sums.txt
```

The installer test serves local fixtures through a fake `curl`; it never fetches
GitHub releases or uses real credentials. The packaged `examples/stdio_smoke.py`
can also run directly against an installed `cupel-acp` using only Python's stdlib.

## Homebrew tap (automated)

The formula bump is AUTOMATED: after every release, the `publish` job in
`release.yml` regenerates `cupel.rb` (via `packaging/update-formula.sh`)
with the new version + checksums and pushes it to the tap repository.
Users install with `brew install rommeld/tap/cupel`.

One-time setup (until then the job skips with a notice, never failing a
release):

1. Create a GitHub repository named `homebrew-tap`; copy
   `packaging/homebrew/cupel.rb` to `Formula/cupel.rb` in it after regenerating
   its version/checksums for a release containing both binaries. The template's
   checked-in historical checksums predate ACP and must not be published as-is.
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
`-p cupel-coding-agent`, then `-p cupel-tui` (dependency order matters;
path dependencies need `version =` fields added first). After that, Rust
users can `cargo install cupel-tui`. Optional - the binary channels above
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
