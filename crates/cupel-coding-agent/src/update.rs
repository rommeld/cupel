//! Is a newer cupel version available? The release check behind the startup notice and
//! `/update`.
//!
//! GitHub's API names the newest published release. Its tag (`v0.32.0`) is the
//! version, and Cargo complied this binary's own version in, so the comparison needs
//! no file on disk and no help from installer. Versions compare as semver: `0.10.0` is
//! newer than `0.9.0`, and `0.30.0-beta1`comes before `0.30.0`.
//!
//! Nothing here knows about the terminal. The TUI decides when to ask and what to
//! show.

use std::time::Duration;

use serde::Deserialize;

/// The version type of this model's API. The re-export lets the TUI name it without a
/// `semver` dependency of its own.
pub use semver::Version;

/// The newest published release. GitHub leaves darfts and prereleases out of it.
pub const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/rommeld/cupel/releases/latest";

/// How long asking GitHub may take. The check runs in the background, but a request
/// that never ends would keep its task alive for the whole session.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// The version this binary was built as.
///
/// Cargo hands every crate the workspace version as `CARGO_PKG_VERSION`, and the
/// release builds sets that version from the tag before it compiles. Cargo accepts
/// only smver versions in a manifest, so the parse cannot fail.
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

/// One file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
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

/// Why a check did not work. The variants say what helps.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("cannot reach GitHub: {0}")]
    Network(String),
    #[error("{0}")]
    BadRelease(String),
}

/// The HTTP client for GitHub, which refuses API requests without a `User-Agent`.
///
/// One client per task, shared by its requests: a `Client` keeps a. pool of open
/// connections, and building one sets up TLS.
#[must_use]
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("cupel/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("the TLS backend initializes")
}

/// The news release, asked form `url`: [`LATEST_RELEASE_URL`], or a local server in
/// the tests.
pub async fn latest_release(client: &reqwest::Client, url: &str) -> Result<Release, UpdateError> {
    // `send` fails without answer; `error_for_status` turns an answer such as 403
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
}
