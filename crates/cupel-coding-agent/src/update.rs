//! Is a newer cupel version available, and putting it in place of this one: the
//! release check behind the startup notice, and the installation behind `/update`.
//!
//! GitHub's API names the newest published release. Its tag (`v0.32.0`) is the
//! version, and Cargo compiled this binary's own version in, so the comparison needs
//! no file on disk and no help from the installer. Versions compare as semver:
//! `0.10.0` is newer than `0.9.0`, and `0.30.0-beta.1` comes before `0.30.0`.
//!
//! [`install`] downloads the archive for this platform and the release's
//! `sha256sums.txt`, compares the checksum, and renames the new binary over the old
//! one. It replaces only a binary that install.sh put in place (its [`INSTALL_MARKER`]
//! sits next to it): Homebrew or a build from source belongs to whoever put it there.
//!
//! Nothing here knows about the terminal. The TUI decides when to ask and what to
//! show.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// The version type of this module's API. The re-export lets the TUI name it without a
/// `semver` dependency of its own.
pub use semver::Version;

/// The newest published release. GitHub leaves drafts and prereleases out of it.
pub const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/rommeld/cupel/releases/latest";

/// How long asking GitHub may take. The check runs in the background, but a request
/// that never ends would keep its task alive for the whole session.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one download may take. An archive has a few megabytes.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Every release publishes this list of checksums next to its archives.
const CHECKSUMS: &str = "sha256sums.txt";

/// install.sh writes this file next to the binary it installs.
pub const INSTALL_MARKER: &str = ".cupel-installer";

/// The one-line install that README.md shows.
const INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh";

/// The version this binary was built as.
///
/// Cargo hands every crate the workspace version as `CARGO_PKG_VERSION`, and the
/// release build sets that version from the tag before it compiles. Cargo accepts
/// only semver versions in a manifest, so the parse cannot fail.
#[must_use]
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo versions are semver")
}

/// A published release: its version and its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub assets: Vec<Asset>,
}

impl Release {
    /// The release's file called `name`.
    fn asset(&self, name: &str) -> Result<&Asset, UpdateError> {
        self.assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or_else(|| {
                UpdateError::BadRelease(format!("cupel {} has no file {name}", self.version))
            })
    }
}

/// One file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    /// Where to download it. GitHub answers with a redirect, which reqwest follows.
    #[serde(rename = "browser_download_url")]
    pub url: String,
}

/// The two fields cupel reads from GitHub's release JSON. serde skips the dozens of
/// others.
#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// Why a check or an update did not work. The variants say what helps.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// GitHub could not be reached, or refused: offline, a timeout, the rate limit.
    /// Trying again later can work.
    #[error("cannot reach GitHub: {0}")]
    Network(String),
    /// The release is not usable as it is: a tag that is no version, a missing file.
    /// Trying again does not help until another release is out.
    #[error("{0}")]
    BadRelease(String),
    /// This binary may not replace itself: another install method, another
    /// platform. The message says what to do instead.
    #[error("{0}")]
    Unsupported(String),
    /// Putting the new binary in place failed: no permission, a full disk, `tar`.
    /// The old binary is untouched.
    #[error("cannot install: {0}")]
    Install(String),
}

/// The HTTP client for GitHub, which refuses API requests without a `User-Agent`.
///
/// One client per task, shared by its requests: a `Client` keeps a pool of open
/// connections, and building one sets up TLS.
#[must_use]
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("cupel/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("the TLS backend initializes")
}

/// The newest release, asked from `url`: [`LATEST_RELEASE_URL`], or a local server in
/// the tests.
pub async fn latest_release(client: &reqwest::Client, url: &str) -> Result<Release, UpdateError> {
    // `send` fails without an answer; `error_for_status` turns an answer such as 403
    // (the rate limit) or 404 into an error too.
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(CHECK_TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(network_error)?;
    let json: ReleaseJson = response.json().await.map_err(|error| {
        UpdateError::BadRelease(format!("GitHub's answer is not a release: {error}"))
    })?;
    // Tags are `v0.30.0`; semver itself has no `v`.
    let tag = json.tag_name.strip_prefix('v').unwrap_or(&json.tag_name);
    let version = Version::parse(tag).map_err(|error| {
        UpdateError::BadRelease(format!(
            "release tag {} is not a version: {error}",
            json.tag_name
        ))
    })?;
    Ok(Release {
        version,
        assets: json.assets,
    })
}

/// The newest release when it is newer than `current`, else `None`.
pub async fn check(
    client: &reqwest::Client,
    url: &str,
    current: &Version,
) -> Result<Option<Release>, UpdateError> {
    let release = latest_release(client, url).await?;
    Ok((release.version > *current).then_some(release))
}

/// A request error as one line. reqwest's own message names only the URL ("error
/// sending request for url (...)"); the reason (refused, time out, no DNS) sits in its
/// chain of sources.
fn network_error(error: reqwest::Error) -> UpdateError {
    // `successors` walks the chain: each cause yields the next, until `source()`
    // returns `None`.
    let causes = std::iter::successors(std::error::Error::source(&error), |cause| cause.source());
    let message = std::iter::once(error.to_string())
        .chain(causes.map(ToString::to_string))
        .collect::<Vec<_>>()
        .join(": ");
    UpdateError::Network(message)
}

/// The release archive for a platform, named by `std::env::consts::OS` and `ARCH`:
/// the platform this binary was compiled for. install.sh picks the same files.
#[must_use]
pub fn archive_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        // One universal binary serves Apple Silicon and Intel.
        ("macos", _) => Some("cupel-macos-universal.tar.gz"),
        ("linux", "x86_64") => Some("cupel-linux-x86_64.tar.gz"),
        ("linux", "aarch64") => Some("cupel-linux-aarch64.tar.gz"),
        _ => None,
    }
}

/// The file this process runs, with symlinks resolved: `~/bin/cupel` may link to
/// `~/.cupel/bin/cupel`, and the file to replace is the one the link points to.
pub fn running_binary() -> Result<PathBuf, UpdateError> {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| {
            UpdateError::Unsupported(format!("cannot find the running cupel: {error}"))
        })
}

/// Why `/update` must leave `binary` alone, or `None` when install.sh put it there.
#[must_use]
pub fn managed_elsewhere(binary: &Path) -> Option<String> {
    if binary
        .parent()
        .is_some_and(|dir| dir.join(INSTALL_MARKER).is_file())
    {
        return None;
    }
    // Homebrew keeps every version in its Cellar and links it into its bin/.
    if binary.components().any(|part| part.as_os_str() == "Cellar") {
        return Some(
            "this cupel comes from Homebrew - update it with: brew upgrade cupel".to_string(),
        );
    }
    Some(format!(
        "{} was not installed by install.sh, so /update leaves it alone - update it \
        the way you installed it, or install cupel with: {INSTALL_COMMAND}",
        binary.display()
    ))
}

/// What `/update` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing: the newest release, this version, is not newer.
    UpToDate(Version),
    /// `binary` is now release `version`. The running process is still the old one.
    Installed { version: Version, binary: PathBuf },
}

/// Put the newest release in place of `binary` when it is newer than `current`.
///
/// The order keeps every failure harmless: ask GitHub, refuse a binary install.sh did
/// not put there, download the archive and the checksum list of that one release,
/// compare, and only then touch the disk. `notify` hears about the download and the
/// installation, for notices.
pub async fn install(
    client: &reqwest::Client,
    url: &str,
    binary: &Path,
    current: &Version,
    notify: impl Fn(String),
) -> Result<Outcome, UpdateError> {
    let release = latest_release(client, url).await?;
    if release.version <= *current {
        return Ok(Outcome::UpToDate(release.version));
    }
    if let Some(how) = managed_elsewhere(binary) {
        return Err(UpdateError::Unsupported(format!(
            "cupel {} is out, but {how}",
            release.version
        )));
    }
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let name = archive_name(os, arch).ok_or_else(|| {
        UpdateError::Unsupported(format!("there is no release archive for {os} {arch}"))
    })?;
    // Archive and checksum list come from the same release: two separate "latest"
    // downloads could straddle a new release.
    let archive = release.asset(name)?;
    let checksums = release.asset(CHECKSUMS)?;

    notify(format!("downloading cupel {} ({name})...", release.version));
    let listed = download(client, checksums).await?;
    let listed = String::from_utf8_lossy(&listed);
    let expected = listed_checksum(&listed, name).ok_or_else(|| {
        UpdateError::BadRelease(format!("{CHECKSUMS} lists no checksum for {name}"))
    })?;
    let bytes = download(client, archive).await?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(UpdateError::BadRelease(format!(
            "{name} does not match its checksum (expected {expected}, got {actual}) - \
            nothing was installed"
        )));
    }

    notify(format!(
        "checksum verified - installing cupel {}...",
        release.version
    ));
    // `replace` runs `tar` and waits for the disk. Blocking work like that belongs on
    // tokio's blocking threads, where it can't stall the async ones.
    let target = binary.to_path_buf();
    tokio::task::spawn_blocking(move || replace(&target, &bytes))
        .await
        .map_err(|error| UpdateError::Install(error.to_string()))??;
    Ok(Outcome::Installed {
        version: release.version,
        binary: binary.to_path_buf(),
    })
}

/// The bytes of `asset`.
async fn download(client: &reqwest::Client, asset: &Asset) -> Result<Vec<u8>, UpdateError> {
    let response = client
        .get(&asset.url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(network_error)?;
    let bytes = response.bytes().await.map_err(network_error)?;
    Ok(bytes.to_vec())
}

/// The checksum that `listed` (a `sha256sums.txt`) gives for the file `name`.
fn listed_checksum<'a>(listed: &'a str, name: &str) -> Option<&'a str> {
    listed.lines().find_map(|line| {
        // sha256sum writes `<hash> <name>`, or `<hash> *<name>` in binary mode.
        let (hash, file) = line.split_once(' ')?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name).then_some(hash)
    })
}

/// The SHA-256 of `bytes` as lowercase hex, the way sha256sum prints it.
fn sha256_hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            // Writing to a String can't fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Put the `cupel` inside `archive` (a release's .tar.gz) in place of `binary`.
///
/// The new file is unpacked into a directory next to `binary` and then renamed over
/// it. A rename inside one file system is atomic: whoever starts cupel gets the old
/// file or the new one, never half of one, and every failure before the rename
/// leaves the old binary as it was. A running process keeps the file it started
/// from (the old file lives on until the process ends), so replacing the binary of
/// this very process is safe.
pub fn replace(binary: &Path, archive: &[u8]) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = binary
        .parent()
        .ok_or_else(|| UpdateError::Install(format!("{} has no directory", binary.display())))?;
    let staging = Staging::create(dir)?;
    let packed = staging.0.join("cupel.tar.gz");
    std::fs::write(&packed, archive).map_err(|error| install_error(&packed, &error))?;
    // `tar` comes with macOS and Linux, and install.sh relies on it too. Only the
    // member `cupel` comes out; README.md and LICENSE stay in the archive.
    let output = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&packed)
        .arg("-C")
        .arg(&staging.0)
        .arg("cupel")
        .output()
        .map_err(|error| UpdateError::Install(format!("cannot run tar: {error}")))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        return Err(UpdateError::Install(format!(
            "tar cannot unpack the archive: {}",
            reason.trim()
        )));
    }
    let unpacked = staging.0.join("cupel");
    std::fs::set_permissions(&unpacked, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| install_error(&unpacked, &error))?;
    std::fs::rename(&unpacked, binary).map_err(|error| install_error(binary, &error))
}

/// A scratch directory next to the binary. `Drop` removes it however `replace`
/// ends: by returning, by `?`, or by a panic.
struct Staging(PathBuf);

impl Staging {
    fn create(dir: &Path) -> Result<Self, UpdateError> {
        // Hidden, and named after this process, so two cupels never share one.
        let path = dir.join(format!(".cupel-update-{}", std::process::id()));
        // A crash can leave one behind, and process ids are reused.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).map_err(|error| {
            UpdateError::Install(format!(
                "cannot write to {}: {error} - /update never uses sudo; reinstall with \
                install.sh as the user who owns that directory",
                dir.display()
            ))
        })?;
        Ok(Self(path))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn install_error(path: &Path, error: &std::io::Error) -> UpdateError {
    UpdateError::Install(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// One route of the fake GitHub: a path, the status and the body it answers with.
    type Route = (String, u16, Vec<u8>);

    /// A local stand-in for GitHub. `routes` gets the server's base URL, so a release
    /// can point its files at the same server. Each request gets the route of its
    /// path, or a 404. The server lives until the test's runtime ends.
    async fn serve(routes: impl FnOnce(&str) -> Vec<Route>) -> String {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(routes(&base));
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                tokio::spawn(async move {
                    let mut socket = tokio::io::BufReader::new(socket);
                    let mut request = String::new();
                    socket.read_line(&mut request).await.unwrap();
                    // The headers end with an empty line ("\r\n", two bytes).
                    let mut header = String::new();
                    while socket.read_line(&mut header).await.unwrap() > 2 {
                        header.clear();
                    }
                    let path = request.split_whitespace().nth(1).unwrap_or("/");
                    let (status, body) = routes
                        .iter()
                        .find(|(route, ..)| route == path)
                        .map_or((404, b"not found".to_vec()), |(_, status, body)| {
                            (*status, body.clone())
                        });
                    let head = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let socket = socket.get_mut();
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                });
            }
        });
        base
    }

    /// GitHub's answer for a release tagged `tag` whose files are `names`, served by
    /// the fake at `base`.
    fn release_json(base: &str, tag: &str, names: &[&str]) -> Vec<u8> {
        let assets: Vec<_> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "browser_download_url": format!("{base}/download/{name}"),
                    "size": 1,
                })
            })
            .collect();
        serde_json::json!({"tag_name": tag, "draft": false, "assets": assets})
            .to_string()
            .into_bytes()
    }

    /// The tests' client: no proxy, so a proxy set on the machine never sees the
    /// requests to 127.0.0.1.
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[tokio::test]
    async fn the_check_compares_versions_as_semver() {
        // (tag of the newest release, version running, newer?)
        let cases = [
            ("v0.30.0", "0.29.3", true),
            ("v0.29.3", "0.29.3", false),
            ("v0.29.2", "0.29.3", false),
            // Compared as text, "0.10.0" would come before "0.9.0".
            ("v0.10.0", "0.9.0", true),
            // A prerelease comes before its release, and they order among themselves.
            ("v0.30.0", "0.30.0-beta.1", true),
            ("v0.30.0-beta.2", "0.30.0-beta.1", true),
            ("v0.30.0-beta.1", "0.30.0", false),
        ];
        for (tag, running, newer) in cases {
            let base = serve(|base| {
                let body = release_json(base, tag, &["sha256sums.txt"]);
                vec![("/releases/latest".to_string(), 200, body)]
            })
            .await;
            let url = format!("{base}/releases/latest");
            let found = check(&test_client(), &url, &version(running))
                .await
                .unwrap();
            assert_eq!(found.is_some(), newer, "{tag} against {running}");
        }
    }

    #[tokio::test]
    async fn the_newest_release_brings_its_version_and_files() {
        let base = serve(|base| {
            let body = release_json(base, "v0.30.0", &["cupel-linux-x86_64.tar.gz"]);
            vec![("/releases/latest".to_string(), 200, body)]
        })
        .await;
        let release = latest_release(&test_client(), &format!("{base}/releases/latest"))
            .await
            .unwrap();
        assert_eq!(release.version, version("0.30.0"));
        assert_eq!(
            release.assets,
            [Asset {
                name: "cupel-linux-x86_64.tar.gz".to_string(),
                url: format!("{base}/download/cupel-linux-x86_64.tar.gz"),
            }]
        );
    }

    #[tokio::test]
    async fn a_strange_answer_is_an_error_not_a_panic() {
        let base = serve(|base| {
            vec![
                (
                    "/nightly".to_string(),
                    200,
                    release_json(base, "nightly", &[]),
                ),
                (
                    "/html".to_string(),
                    200,
                    b"<html>maintenance</html>".to_vec(),
                ),
                ("/limited".to_string(), 403, b"rate limit exceeded".to_vec()),
            ]
        })
        .await;
        let client = test_client();
        let result = latest_release(&client, &format!("{base}/nightly")).await;
        assert!(
            matches!(&result, Err(UpdateError::BadRelease(message)) if message.contains("nightly")),
            "{result:?}"
        );
        let result = latest_release(&client, &format!("{base}/html")).await;
        assert!(
            matches!(result, Err(UpdateError::BadRelease(_))),
            "{result:?}"
        );
        let result = latest_release(&client, &format!("{base}/limited")).await;
        assert!(
            matches!(&result, Err(UpdateError::Network(message)) if message.contains("403")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn offline_is_a_network_error() {
        // A port that was free a moment ago: nothing listens there now.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let result = latest_release(&test_client(), &format!("http://{address}/latest")).await;
        assert!(matches!(result, Err(UpdateError::Network(_))), "{result:?}");
    }

    #[tokio::test]
    async fn a_github_that_never_answers_times_out() {
        // The server takes the connection and then says nothing.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        // The outer timeout turns a missing CHECK_TIMEOUT into a failure, not a hang.
        let result = tokio::time::timeout(
            CHECK_TIMEOUT * 3,
            latest_release(&test_client(), &format!("http://{address}/latest")),
        )
        .await
        .expect("CHECK_TIMEOUT ends the request");
        assert!(matches!(result, Err(UpdateError::Network(_))), "{result:?}");
    }

    /// An empty directory of its own for each test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-update-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A release archive packed like release.yml packs it, with a `cupel` that is a
    /// script printing `text`.
    fn archive(name: &str, text: &str) -> Vec<u8> {
        let dir = temp_dir(&format!("{name}-archive"));
        std::fs::write(dir.join("cupel"), format!("#!/bin/sh\necho {text}\n")).unwrap();
        std::fs::write(dir.join("README.md"), "read me").unwrap();
        let packed = dir.join("packed.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&packed)
            .arg("-C")
            .arg(&dir)
            .args(["cupel", "README.md"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(packed).unwrap()
    }

    /// A cupel that install.sh installed: the binary and the marker next to it.
    fn installed(name: &str) -> PathBuf {
        let dir = temp_dir(name);
        std::fs::write(dir.join(INSTALL_MARKER), "install.sh").unwrap();
        let binary = dir.join("cupel");
        std::fs::write(&binary, "old").unwrap();
        binary
    }

    /// Whatever `replace` leaves in the binary's directory besides the binary and
    /// the marker.
    fn leftovers(binary: &Path) -> Vec<String> {
        std::fs::read_dir(binary.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "cupel" && name != INSTALL_MARKER)
            .collect()
    }

    #[test]
    fn each_platform_gets_its_archive() {
        let macos = Some("cupel-macos-universal.tar.gz");
        assert_eq!(archive_name("macos", "aarch64"), macos);
        assert_eq!(archive_name("macos", "x86_64"), macos);
        let linux = archive_name("linux", "x86_64");
        assert_eq!(linux, Some("cupel-linux-x86_64.tar.gz"));
        let arm = archive_name("linux", "aarch64");
        assert_eq!(arm, Some("cupel-linux-aarch64.tar.gz"));
        assert_eq!(archive_name("linux", "riscv64"), None);
        assert_eq!(archive_name("windows", "x86_64"), None);
    }

    #[test]
    fn checksums_are_read_the_way_sha256sum_writes_them() {
        let listed = "aaa  cupel-linux-x86_64.tar.gz.sig\n\
            bbb  cupel-linux-x86_64.tar.gz\n\
            ccc *cupel-macos-universal.tar.gz\n";
        // The whole name must match, not a part of it.
        let found = listed_checksum(listed, "cupel-linux-x86_64.tar.gz");
        assert_eq!(found, Some("bbb"));
        let found = listed_checksum(listed, "cupel-macos-universal.tar.gz");
        assert_eq!(found, Some("ccc"));
        assert_eq!(listed_checksum(listed, "cupel-linux-aarch64.tar.gz"), None);
        // The SHA-256 of "abc" from FIPS 180-2.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn only_a_binary_from_install_sh_may_be_replaced() {
        let binary = installed("marker");
        assert_eq!(managed_elsewhere(&binary), None);

        std::fs::remove_file(binary.with_file_name(INSTALL_MARKER)).unwrap();
        let how = managed_elsewhere(&binary).unwrap();
        assert!(how.contains("not installed by install.sh"), "{how}");
        assert!(how.contains(INSTALL_COMMAND), "{how}");

        let brew = Path::new("/opt/homebrew/Cellar/cupel/0.29.3/bin/cupel");
        let how = managed_elsewhere(brew).unwrap();
        assert!(how.contains("brew upgrade cupel"), "{how}");
    }

    #[test]
    fn replace_swaps_the_binary_and_leaves_nothing_behind() {
        use std::os::unix::fs::PermissionsExt as _;
        let binary = installed("replace");
        replace(&binary, &archive("replace", "new")).unwrap();
        let script = std::fs::read_to_string(&binary).unwrap();
        assert_eq!(script, "#!/bin/sh\necho new\n");
        let mode = std::fs::metadata(&binary).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(leftovers(&binary).is_empty(), "{:?}", leftovers(&binary));
    }

    #[test]
    fn a_broken_archive_leaves_the_old_binary() {
        let binary = installed("broken");
        let result = replace(&binary, b"no archive at all");
        assert!(
            matches!(&result, Err(UpdateError::Install(message)) if message.contains("tar")),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "old");
        assert!(leftovers(&binary).is_empty(), "{:?}", leftovers(&binary));
    }

    #[test]
    fn a_directory_without_write_permission_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let binary = installed("read-only");
        let dir = binary.parent().unwrap();
        let read_only = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(dir, read_only).unwrap();
        let result = replace(&binary, &archive("read-only", "new"));
        // Writable again, so the next run can clean up.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(&result, Err(UpdateError::Install(message)) if message.contains("sudo")),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "old");
    }

    /// A fake GitHub with release `tag`: every platform's archive is `packed`, and
    /// `sha256sums.txt` lists `checksum` for each.
    async fn release_server(tag: &str, packed: Vec<u8>, checksum: &str) -> String {
        let names = [
            "cupel-linux-aarch64.tar.gz",
            "cupel-linux-x86_64.tar.gz",
            "cupel-macos-universal.tar.gz",
        ];
        let listed = names
            .iter()
            .map(|name| format!("{checksum}  {name}"))
            .collect::<Vec<_>>()
            .join("\n");
        serve(|base| {
            let mut all = names.to_vec();
            all.push(CHECKSUMS);
            let mut routes = vec![
                ("/latest".to_string(), 200, release_json(base, tag, &all)),
                (format!("/download/{CHECKSUMS}"), 200, listed.into_bytes()),
            ];
            for name in names {
                routes.push((format!("/download/{name}"), 200, packed.clone()));
            }
            routes
        })
        .await
    }

    #[tokio::test]
    async fn install_puts_a_newer_release_in_place() {
        let packed = archive("install", "new");
        let checksum = sha256_hex(&packed);
        let base = release_server("v0.30.0", packed, &checksum).await;
        let binary = installed("install");
        let notes = std::sync::Mutex::new(Vec::new());
        let outcome = install(
            &test_client(),
            &format!("{base}/latest"),
            &binary,
            &version("0.29.3"),
            |note| notes.lock().unwrap().push(note),
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            Outcome::Installed {
                version: version("0.30.0"),
                binary: binary.clone(),
            }
        );
        assert!(std::fs::read_to_string(&binary).unwrap().contains("new"));
        let notes = notes.into_inner().unwrap();
        let name = archive_name(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        assert_eq!(
            notes,
            [
                format!("downloading cupel 0.30.0 ({name})..."),
                "checksum verified - installing cupel 0.30.0...".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn install_refuses_missing_release_files() {
        let name = archive_name(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let binary = installed("missing-files");
        for (present, missing) in [(CHECKSUMS, name), (name, CHECKSUMS)] {
            let base = serve(|base| {
                vec![(
                    "/latest".to_string(),
                    200,
                    release_json(base, "v0.30.0", &[present]),
                )]
            })
            .await;
            let result = install(
                &test_client(),
                &format!("{base}/latest"),
                &binary,
                &version("0.29.3"),
                drop,
            )
            .await;
            assert!(
                matches!(&result, Err(UpdateError::BadRelease(message))
                    if message == &format!("cupel 0.30.0 has no file {missing}")),
                "{result:?}"
            );
            assert_eq!(std::fs::read_to_string(&binary).unwrap(), "old");
            assert!(leftovers(&binary).is_empty(), "{:?}", leftovers(&binary));
        }
    }

    #[tokio::test]
    async fn install_refuses_a_missing_checksum_before_downloading_the_archive() {
        let name = archive_name(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        // No archive route: trying to download it would give a Network error.
        let base = serve(|base| {
            vec![
                (
                    "/latest".to_string(),
                    200,
                    release_json(base, "v0.30.0", &[name, CHECKSUMS]),
                ),
                (
                    format!("/download/{CHECKSUMS}"),
                    200,
                    b"abc  some-other-file.tar.gz\n".to_vec(),
                ),
            ]
        })
        .await;
        let binary = installed("missing-checksum");
        let result = install(
            &test_client(),
            &format!("{base}/latest"),
            &binary,
            &version("0.29.3"),
            drop,
        )
        .await;
        assert!(
            matches!(&result, Err(UpdateError::BadRelease(message))
                if message == &format!("{CHECKSUMS} lists no checksum for {name}")),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "old");
        assert!(leftovers(&binary).is_empty(), "{:?}", leftovers(&binary));
    }

    #[tokio::test]
    async fn install_refuses_a_checksum_mismatch() {
        let packed = archive("mismatch", "new");
        let base = release_server("v0.30.0", packed, &"0".repeat(64)).await;
        let binary = installed("mismatch");
        let url = format!("{base}/latest");
        let result = install(&test_client(), &url, &binary, &version("0.29.3"), drop).await;
        assert!(
            matches!(&result, Err(UpdateError::BadRelease(message))
                if message.contains("nothing was installed")),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "old");
    }

    #[tokio::test]
    async fn install_changes_nothing_without_a_newer_release_or_the_marker() {
        let packed = archive("unchanged", "new");
        let checksum = sha256_hex(&packed);
        let base = release_server("v0.30.0", packed, &checksum).await;
        let url = format!("{base}/latest");
        let binary = installed("unchanged");

        let outcome = install(&test_client(), &url, &binary, &version("0.30.0"), drop).await;
        assert_eq!(outcome.unwrap(), Outcome::UpToDate(version("0.30.0")));

        std::fs::remove_file(binary.with_file_name(INSTALL_MARKER)).unwrap();
        let result = install(&test_client(), &url, &binary, &version("0.29.3"), drop).await;
        assert!(
            matches!(&result, Err(UpdateError::Unsupported(message))
                if message.starts_with("cupel 0.30.0 is out, but")),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "old");
    }
}
