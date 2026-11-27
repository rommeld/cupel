# Releasing and packaging cupel

Each release has three platform archives. Starting with the ACP packaging
release, each contains `cupel`, `cupel-acp`, `LICENSE`, `README.md`, and ACP client
examples under `examples/`. Archive names and the published `sha256sums.txt`
stay unchanged, so existing Cupel installers and `/update` remain compatible.

## Automatic releases

Before merging a PR from this repository's `develop` branch into `main`, post
one of these as a standalone comment in the PR conversation:

| PR comment | Version bump |
| --- | --- |
| `Changelog: added` | Minor (`0.53.0` → `0.54.0`) |
| `Changelog: fixed` | Patch (`0.53.0` → `0.53.1`) |
| `Changelog: refact` | Patch (`0.53.0` → `0.53.1`) |

The workflow reads comments from repository owners, members, and collaborators
that were last updated before or at merge time. `added` takes precedence if
both bump types are present. Without a matching comment, no release is created.
If a newer release already includes the merge, the workflow skips it.

The workflow bumps from the highest stable version tag (or the workspace
version if none exists). It commits `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md`
on top of the merge commit and pushes only the tag. Neither branch is changed.
If the run fails after pushing the tag, rerun it to reuse the same version.

The workflow builds both executables as macOS universal binaries and Linux musl
binaries. It publishes the archives and `sha256sums.txt` as a full GitHub
Release, not a prerelease. `install.sh` downloads the latest release:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

Release publication is retryable: an interrupted draft resumes its uploads.
Release runs are serialized.

### Manual releases

You can still push a `v*` tag to trigger a release. Choose a version above the
latest release and prepare it in a checkout with full history and tags:

```sh
sh packaging/prepare-release.sh v0.54.1
```

If the previous version was tagged by CI, first copy its `CHANGELOG.md` into
the checkout (`git show v0.54.0:CHANGELOG.md > CHANGELOG.md`) and pass its tag
as the second argument to the script. Review, test, and commit the metadata,
then run `git tag v0.54.1 && git push origin v0.54.1`. Release builds verify
that the tag matches the committed workspace version and changelog section.

Re-running a release uses the workflow definition from the original run.

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

## macOS signing and notarization (optional)

Unsigned binaries downloaded via a BROWSER are blocked by Gatekeeper until
`xattr -d com.apple.quarantine cupel`. Installs via `install.sh` or
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
(`dist init`) can generate an equivalent workflow plus installers from
Cargo.toml metadata, and keeps them updated with the tool.
Migrate when maintaining the YAML by hand stops being instructive:
delete `release.yml` + `install.sh`, run `dist init`, commit its output.

## Platform support

| Platform | Status |
| --- | --- |
| macOS (Apple Silicon + Intel) | universal binary |
| Linux x86_64 / aarch64 | static musl binaries, any distro (kill/timeout handling verified by CI since v0.1.1-beta) |
| Windows | not yet: the bash tool is Unix-only (process groups, `kill`) |
