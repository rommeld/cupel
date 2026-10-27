# Releasing and packaging cupel

Each release has three platform archives. Starting with the ACP packaging
release, each contains `cupel`, `cupel-acp`, `LICENSE`, `README.md`, and ACP client
examples under `examples/`. Archive names and the published `sha256sums.txt`
stay unchanged, so existing Cupel installers and `/update` remain compatible.

## Cutting a release

Prepare the changelog and workspace version on a branch with full git history
and tags. The helper only edits `CHANGELOG.md`, `Cargo.toml`, and `Cargo.lock`;
it does not commit, push, or create a tag:

```sh
sh packaging/prepare-release.sh v0.33.11
```

Review and commit those files in the release PR. CI runs formatting, strict
Clippy, Rust tests, and packaging tests on Ubuntu and macOS using the pinned
`rust-toolchain.toml`. Once that PR is green and merged, tag the merged commit:

```sh
git switch main && git pull --ff-only
git tag v0.33.11 && git push origin v0.33.11
```

The tag must match the committed workspace version and have a changelog section;
release builds check both. Tests run in the PR, not again for the tag.

`.github/workflows/release.yml` builds both executables as macOS universal
binaries and Linux musl binaries, and publishes a GitHub Release with the archives and a
`sha256sums.txt`. The release is created as a FULL release (not a
prerelease), which is what makes `releases/latest/download/` - and
therefore `install.sh` - point at it. Users install with:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

Release publication is retryable: an interrupted draft resumes its uploads.
Publishing is serialized, and only GitHub's latest release updates Homebrew.
The workflow never writes to `main`, opens bookkeeping PRs, or bypasses branch
protection: metadata is already committed in the tested release PR.

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
