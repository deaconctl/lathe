use crate::ReleaseAsset;
use anyhow::{Context as _, Result};
use http_client::{
    HttpClient, HttpClientWithUrl, HttpRequestExt, RedirectPolicy, Request, Url, http::HeaderMap,
};
use release_channel::ReleaseChannel;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use smol::io::AsyncReadExt;
use std::{env, sync::Arc, time::Duration};

/// GitHub API URL for Lathe releases, resolved per channel.
///
/// - `LATHE_BETA_UPDATE_URL` overrides the URL for Beta builds.
/// - `LATHE_UPDATE_URL` overrides the URL for Stable/Preview/Beta builds.
///
/// Both are read from `option_env!` (so CI can bake a different URL into the
/// binary at build time) with a runtime `env::var` fallback for development.
/// When neither is set, Stable/Preview/Beta default to the Lathe fork's
/// GitHub releases endpoint so locally built binaries still get update
/// checks without any environment configuration. Nightly/Dev never poll.
const LATHE_DEFAULT_UPDATE_URL: &str =
    "https://api.github.com/repos/paterschris/lathe/releases?per_page=30";

pub const PACKAGE_MANAGER_INSTALL_TITLE: &str = "Lathe was installed via a package manager.";
pub const UP_TO_DATE_TITLE: &str = "Lathe is up to date";
pub const UP_TO_DATE_MESSAGE: &str = "You're already running the latest version.";

pub fn update_base_url(channel: ReleaseChannel) -> Option<String> {
    let beta = option_env!("LATHE_BETA_UPDATE_URL")
        .map(str::to_owned)
        .or_else(|| env::var("LATHE_BETA_UPDATE_URL").ok())
        .filter(|s| !s.is_empty());
    let generic = option_env!("LATHE_UPDATE_URL")
        .map(str::to_owned)
        .or_else(|| env::var("LATHE_UPDATE_URL").ok())
        .filter(|s| !s.is_empty());
    match channel {
        ReleaseChannel::Beta => Some(
            beta.or(generic)
                .unwrap_or_else(|| LATHE_DEFAULT_UPDATE_URL.to_owned()),
        ),
        ReleaseChannel::Stable | ReleaseChannel::Preview => {
            Some(generic.unwrap_or_else(|| LATHE_DEFAULT_UPDATE_URL.to_owned()))
        }
        ReleaseChannel::Nightly | ReleaseChannel::Dev => None,
    }
}

/// Optional HTML URL for release notes (e.g. the GitHub releases HTML page).
/// Falls back to a naive transform of the API URL, then to `None`.
pub fn release_notes_url(channel: ReleaseChannel) -> Option<String> {
    if let Some(explicit) = option_env!("LATHE_RELEASE_NOTES_URL")
        .map(str::to_owned)
        .or_else(|| env::var("LATHE_RELEASE_NOTES_URL").ok())
        .filter(|s| !s.is_empty())
    {
        return Some(explicit);
    }
    let api = update_base_url(channel)?;
    // api.github.com/repos/OWNER/REPO/releases/... -> github.com/OWNER/REPO/releases
    api.strip_prefix("https://api.github.com/repos/")
        .and_then(|rest| rest.split_once("/releases"))
        .map(|(owner_repo, _)| format!("https://github.com/{owner_repo}/releases"))
        .or(Some(api))
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize, Clone)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// The tag suffix used by the release workflow for each non-stable channel.
/// Stable has no suffix. Dev has no published releases.
fn channel_tag_suffix(channel: ReleaseChannel) -> Option<&'static str> {
    match channel {
        ReleaseChannel::Stable => Some(""),
        ReleaseChannel::Preview => Some("-preview"),
        ReleaseChannel::Beta => Some("-beta"),
        ReleaseChannel::Nightly => Some("-nightly"),
        ReleaseChannel::Dev => None,
    }
}

fn release_matches_channel(release: &GitHubRelease, channel: ReleaseChannel) -> bool {
    if release.draft {
        return false;
    }
    let Some(suffix) = channel_tag_suffix(channel) else {
        return false;
    };
    if suffix.is_empty() {
        // Stable: tag has no `-foo` suffix at all (e.g. `v0.236.0`).
        !release.tag_name.contains('-')
    } else {
        release.tag_name.ends_with(suffix)
    }
}

/// The release notes for a single published Lathe release.
pub struct ReleaseNotes {
    /// The release name, or its tag when the release has no name.
    pub title: String,
    /// The release body, as Markdown.
    pub notes: String,
}

/// Fetches the release notes for `version` on `channel` from the GitHub
/// releases API. Falls back to the newest release matching the channel when
/// no release is tagged for the running version, which happens for locally
/// built binaries whose version was never published.
pub async fn get_release_notes(
    http_client: Arc<HttpClientWithUrl>,
    api_url: &str,
    channel: ReleaseChannel,
    version: &str,
) -> Result<ReleaseNotes> {
    let releases = get_releases(http_client, api_url, None).await?;
    let suffix = channel_tag_suffix(channel).unwrap_or_default();
    let version = version.trim_start_matches('v');
    let tag = format!("v{version}{suffix}");

    let release = releases
        .iter()
        .find(|release| !release.draft && release.tag_name == tag)
        .or_else(|| {
            releases
                .iter()
                .find(|release| release_matches_channel(release, channel))
        })
        .with_context(|| format!("no release notes found for {tag} at {api_url}"))?;

    let notes = release
        .body
        .clone()
        .filter(|body| !body.trim().is_empty())
        .with_context(|| format!("release {} has no release notes", release.tag_name))?;

    Ok(ReleaseNotes {
        title: release
            .name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| release.tag_name.clone()),
        notes,
    })
}

pub struct UpdatePing {
    pub installation_id: Option<Arc<str>>,
    pub metrics_enabled: bool,
    pub version: String,
    pub channel: ReleaseChannel,
    pub os: String,
    pub arch: String,
}

fn update_headers(api_url: &str, ping: Option<&UpdatePing>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if Url::parse(api_url)?.host_str() == Some("api.github.com") {
        return Ok(headers);
    }
    let Some(ping) = ping else {
        return Ok(headers);
    };
    for (name, value) in [
        ("x-lathe-version", ping.version.as_str()),
        ("x-lathe-channel", ping.channel.dev_name()),
        ("x-lathe-os", ping.os.as_str()),
        ("x-lathe-arch", ping.arch.as_str()),
    ] {
        headers.insert(name, value.parse()?);
    }
    if ping.metrics_enabled {
        if let Some(installation_id) = ping.installation_id.as_deref().filter(|id| !id.is_empty()) {
            let mut digest = Sha256::new();
            digest.update(b"lathe-update-ping-v1");
            digest.update(installation_id.as_bytes());
            headers.insert(
                "x-lathe-install",
                format!("{:x}", digest.finalize()).parse()?,
            );
        }
    }
    Ok(headers)
}

async fn get_releases(
    http_client: Arc<HttpClientWithUrl>,
    api_url: &str,
    ping: Option<&UpdatePing>,
) -> Result<Vec<GitHubRelease>> {
    let headers = update_headers(api_url, ping)?;
    // Do not forward the anonymous install header through redirects to other hosts.
    let mut request = Request::builder()
        .uri(api_url)
        .follow_redirects(RedirectPolicy::NoFollow)
        .timeout(Duration::from_secs(15))
        .body(Default::default())?;
    *request.headers_mut() = headers;
    let response = http_client.send(request).await;
    let should_fallback = api_url != LATHE_DEFAULT_UPDATE_URL
        && response
            .as_ref()
            .map_or(true, |response| response.status().is_server_error());
    let mut response = if should_fallback {
        http_client
            .get(LATHE_DEFAULT_UPDATE_URL, Default::default(), true)
            .await?
    } else {
        response?
    };
    let mut body = Vec::new();
    if let Err(error) = response.body_mut().read_to_end(&mut body).await {
        if api_url == LATHE_DEFAULT_UPDATE_URL || should_fallback {
            return Err(error.into());
        }
        response = http_client
            .get(LATHE_DEFAULT_UPDATE_URL, Default::default(), true)
            .await?;
        body.clear();
        response.body_mut().read_to_end(&mut body).await?;
    }

    anyhow::ensure!(
        response.status().is_success(),
        "failed to fetch release from {api_url}: {:?}",
        String::from_utf8_lossy(&body),
    );

    // Accept either response shape so old binaries with `/releases/latest`
    // baked in keep working, while newer binaries can be pointed at the
    // `/releases` list endpoint to discover prerelease channels (beta,
    // preview) that `/latest` excludes. List responses are returned newest
    // first by the GitHub API, so the first match wins.
    if let Ok(single) = serde_json::from_slice::<GitHubRelease>(&body) {
        Ok(vec![single])
    } else {
        serde_json::from_slice(&body).with_context(|| {
            format!(
                "error deserializing release(s) {:?}",
                String::from_utf8_lossy(&body),
            )
        })
    }
}

pub async fn get_release_asset(
    http_client: Arc<HttpClientWithUrl>,
    api_url: &str,
    channel: ReleaseChannel,
    os: &str,
    arch: &str,
    prefer_appimage: bool,
    ping: &UpdatePing,
) -> Result<ReleaseAsset> {
    let releases = get_releases(http_client, api_url, Some(ping)).await?;
    let release = releases
        .into_iter()
        .find(|release| release_matches_channel(release, channel))
        .with_context(|| format!("no release matched channel {:?} at {api_url}", channel))?;

    // Asset naming produced by the release workflow:
    //   macOS:   Lathe-<version>-<arch>-macos.{dmg,zip}    (arch: aarch64 | x86_64)
    //   Linux:   Lathe-<version>-<arch>-linux.{tar.gz,AppImage}
    //   Windows: Lathe-<version>-<arch>.exe
    // macOS prefers .dmg when available (used by the in-app installer's
    // hdiutil mount flow); .zip is accepted as a fallback. Linux AppImage
    // installs never fall back to the tarball, since installing it would
    // leave a second copy alongside the AppImage being run.
    let matches = |asset: &GitHubAsset, ext: &str| {
        asset.name.ends_with(ext)
            && asset.name.contains(arch)
            && (os != "linux" || asset.name.contains("linux"))
    };

    let asset = match os {
        "macos" => release
            .assets
            .iter()
            .find(|a| matches(a, ".dmg"))
            .or_else(|| release.assets.iter().find(|a| matches(a, ".zip")))
            .cloned(),
        "linux" if prefer_appimage => release
            .assets
            .iter()
            .find(|a| matches(a, ".AppImage"))
            .cloned(),
        "linux" => release
            .assets
            .iter()
            .find(|a| matches(a, ".tar.gz"))
            .cloned(),
        "windows" => release.assets.iter().find(|a| matches(a, ".exe")).cloned(),
        other => anyhow::bail!("unsupported os: {other}"),
    }
    .with_context(|| {
        format!(
            "no matching asset for os={os} arch={arch} in release {}",
            release.tag_name
        )
    })?;

    // tag_name like "v0.234.7" -> semver "0.234.7".
    let version = release.tag_name.trim_start_matches('v').to_string();

    Ok(ReleaseAsset {
        version,
        url: asset.browser_download_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping() -> UpdatePing {
        UpdatePing {
            installation_id: Some(Arc::from("test-install")),
            metrics_enabled: true,
            version: "0.236.0".into(),
            channel: ReleaseChannel::Stable,
            os: "macos".into(),
            arch: "aarch64".into(),
        }
    }

    #[test]
    fn opted_in_headers() -> Result<()> {
        let headers = update_headers("https://updates.example/releases", Some(&ping()))?;
        assert_eq!(headers["x-lathe-version"], "0.236.0");
        assert_eq!(headers["x-lathe-channel"], "stable");
        assert_eq!(headers["x-lathe-os"], "macos");
        assert_eq!(headers["x-lathe-arch"], "aarch64");
        let expected = format!("{:x}", Sha256::digest(b"lathe-update-ping-v1test-install"));
        assert_eq!(headers["x-lathe-install"], expected);
        assert_eq!(headers["x-lathe-install"].as_bytes().len(), 64);
        Ok(())
    }

    #[test]
    fn opted_out_headers() -> Result<()> {
        let mut ping = ping();
        ping.metrics_enabled = false;
        let headers = update_headers("https://updates.example/releases", Some(&ping))?;
        assert!(!headers.contains_key("x-lathe-install"));
        assert_eq!(headers.len(), 4);
        Ok(())
    }

    #[test]
    fn missing_installation_id_headers() -> Result<()> {
        let mut ping = ping();
        ping.installation_id = None;
        let headers = update_headers("https://updates.example/releases", Some(&ping))?;
        assert!(!headers.contains_key("x-lathe-install"));
        assert_eq!(headers.len(), 4);
        Ok(())
    }

    #[test]
    fn github_has_no_custom_headers() -> Result<()> {
        for url in [LATHE_DEFAULT_UPDATE_URL, "https://API.GITHUB.COM/releases"] {
            assert!(update_headers(url, Some(&ping()))?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn proxy_failure_retries_github_without_headers() -> Result<()> {
        use http_client::{FakeHttpClient, Response};
        use std::sync::atomic::{AtomicUsize, Ordering};

        for status in [None, Some(500), Some(503)] {
            let requests = Arc::new(AtomicUsize::new(0));
            let client = FakeHttpClient::create({
                let requests = requests.clone();
                move |request| {
                    let number = requests.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if number == 0 {
                            assert_eq!(request.uri().host(), Some("updates.example"));
                            assert_eq!(
                                request.extensions().get::<RedirectPolicy>(),
                                Some(&RedirectPolicy::NoFollow)
                            );
                            assert!(request.headers().contains_key("x-lathe-install"));
                            if let Some(status) = status {
                                Ok(Response::builder()
                                    .status(status)
                                    .body(Default::default())?)
                            } else {
                                anyhow::bail!("offline")
                            }
                        } else {
                            assert_eq!(number, 1);
                            assert_eq!(request.uri().to_string(), LATHE_DEFAULT_UPDATE_URL);
                            assert!(
                                !request
                                    .headers()
                                    .keys()
                                    .any(|name| name.as_str().starts_with("x-lathe-"))
                            );
                            Ok(Response::builder().status(200).body("[]".into())?)
                        }
                    }
                }
            });
            let releases = smol::block_on(get_releases(
                client,
                "https://updates.example/releases",
                Some(&ping()),
            ))?;
            assert!(releases.is_empty());
            assert_eq!(requests.load(Ordering::SeqCst), 2);
        }
        Ok(())
    }

    #[test]
    fn proxy_client_error_does_not_retry() {
        use http_client::{FakeHttpClient, Response};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let requests = Arc::new(AtomicUsize::new(0));
        let client = FakeHttpClient::create({
            let requests = requests.clone();
            move |_| {
                requests.fetch_add(1, Ordering::SeqCst);
                async { Ok(Response::builder().status(403).body(Default::default())?) }
            }
        });
        assert!(
            smol::block_on(get_releases(
                client,
                "https://updates.example/releases",
                Some(&ping()),
            ))
            .is_err()
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
