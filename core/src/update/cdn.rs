//! Zero-API release downloads over the GitHub Releases CDN.
//!
//! Public repositories expose the latest release's assets at
//! `https://github.com/<owner>/<repo>/releases/latest/download/<asset>`.
//! These routes are anonymously readable, served by GitHub's CDN, and do
//! not consume api.github.com rate limits (which anonymous callers
//! exhaust quickly). All update traffic goes through here.

use futures_util::StreamExt;
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode};
use url::Url;

use crate::update::UpdateError;

/// Hosts GitHub's release CDN legitimately redirects to. Anything else
/// is rejected so a compromised or misconfigured route cannot push the
/// updater to an arbitrary origin.
const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

/// Base URL of the anonymous CDN routes for the latest release.
pub fn cdn_base_url(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}/releases/latest/download")
}

/// Human-readable releases page (for "open in browser" links).
pub fn releases_page_url(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}/releases")
}

/// An asset downloaded from the latest release.
#[derive(Clone, Debug)]
pub struct ReleaseAssetDownload {
    pub bytes: Vec<u8>,
    /// URL of the releases page, for display purposes.
    pub release_url: String,
}

#[derive(Clone)]
pub struct ReleaseCdn {
    client: Client,
    base: String,
    releases_page: String,
}

impl ReleaseCdn {
    pub fn new(owner: &str, repo: &str) -> Result<Self, UpdateError> {
        Self::with_base(cdn_base_url(owner, repo), releases_page_url(owner, repo))
    }

    fn with_base(base: String, releases_page: String) -> Result<Self, UpdateError> {
        let client = Client::builder()
            .user_agent(concat!("pandock-updater/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::custom(|attempt| {
                let host = attempt.url().host_str().unwrap_or_default();
                if ALLOWED_HOSTS.contains(&host) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        Ok(Self {
            client,
            base,
            releases_page,
        })
    }

    /// Download an asset (by file name) from the latest release.
    /// Returns `Ok(None)` when the latest release does not carry that
    /// asset (HTTP 404), which callers treat as "no update information".
    pub async fn download_latest_asset(
        &self,
        asset_name: &str,
        max_size: u64,
    ) -> Result<Option<ReleaseAssetDownload>, UpdateError> {
        let url = format!("{}/{asset_name}", self.base);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|error| Self::map_reqwest_error(&error))?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(UpdateError::RateLimited(None));
        }
        if !status.is_success() {
            let host = Url::parse(response.url().as_str())
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_string()))
                .unwrap_or_default();
            return Err(UpdateError::Network(format!(
                "HTTP {} from {host}",
                status.as_u16()
            )));
        }
        if let Some(length) = response.content_length() {
            if length > max_size {
                return Err(UpdateError::InvalidArchive(format!(
                    "资产 {asset_name} 超出大小上限（{length} > {max_size}）"
                )));
            }
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| Self::map_reqwest_error(&error))?;
            if bytes.len() as u64 + chunk.len() as u64 > max_size {
                return Err(UpdateError::InvalidArchive(format!(
                    "资产 {asset_name} 超出大小上限 {max_size}"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(ReleaseAssetDownload {
            bytes,
            release_url: self.releases_page.clone(),
        }))
    }

    fn map_reqwest_error(error: &reqwest::Error) -> UpdateError {
        if error.is_redirect() {
            UpdateError::Network(format!("重定向目标不在允许列表：{error}"))
        } else {
            UpdateError::Network(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_cdn(server: &MockServer) -> ReleaseCdn {
        ReleaseCdn::with_base(server.uri(), "https://example.invalid/releases".into())
            .expect("client")
    }

    #[test]
    fn cdn_urls_target_latest_download_routes() {
        assert_eq!(
            cdn_base_url("weyham", "pandock"),
            "https://github.com/weyham/pandock/releases/latest/download"
        );
        assert_eq!(
            releases_page_url("weyham", "pandock"),
            "https://github.com/weyham/pandock/releases"
        );
    }

    #[test]
    fn client_builds_for_real_repo() {
        let cdn = ReleaseCdn::new("weyham", "pandock").expect("client");
        assert!(cdn.base.ends_with("/releases/latest/download"));
    }

    #[tokio::test]
    async fn downloads_asset_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases.win.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"assets\":[]}"))
            .mount(&server)
            .await;
        let download = test_cdn(&server)
            .download_latest_asset("releases.win.json", 1024)
            .await
            .expect("download")
            .expect("asset present");
        assert_eq!(download.bytes, b"{\"assets\":[]}");
        assert_eq!(download.release_url, "https://example.invalid/releases");
    }

    #[tokio::test]
    async fn missing_asset_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let result = test_cdn(&server)
            .download_latest_asset("nope.zip", 1024)
            .await
            .expect("404 is not an error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn forbidden_and_429_map_to_rate_limited() {
        for status in [403u16, 429u16] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let result = test_cdn(&server).download_latest_asset("a.zip", 1024).await;
            assert!(
                matches!(result, Err(UpdateError::RateLimited(_))),
                "status {status} should map to RateLimited"
            );
        }
    }

    #[tokio::test]
    async fn server_error_maps_to_network_with_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let result = test_cdn(&server).download_latest_asset("a.zip", 1024).await;
        match result {
            Err(UpdateError::Network(message)) => assert!(message.contains("HTTP 500")),
            other => panic!("expected Network error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn content_length_over_cap_is_rejected_before_streaming() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 4096]))
            .mount(&server)
            .await;
        let result = test_cdn(&server)
            .download_latest_asset("big.zip", 1024)
            .await;
        assert!(matches!(result, Err(UpdateError::InvalidArchive(_))));
    }

    #[tokio::test]
    async fn redirect_to_disallowed_host_is_rejected() {
        let server = MockServer::start().await;
        // 127.0.0.1 is deliberately absent from ALLOWED_HOSTS.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "http://127.0.0.1:1/evil"),
            )
            .mount(&server)
            .await;
        let result = test_cdn(&server).download_latest_asset("a.zip", 1024).await;
        // Policy stops the redirect; the 3xx response then fails the
        // success check and surfaces as a network error.
        assert!(matches!(result, Err(UpdateError::Network(_))));
    }

    #[tokio::test]
    async fn connect_failure_maps_to_network() {
        let cdn =
            ReleaseCdn::with_base("http://127.0.0.1:1".into(), String::new()).expect("client");
        let result = cdn.download_latest_asset("a.zip", 1024).await;
        assert!(matches!(result, Err(UpdateError::Network(_))));
    }
}
