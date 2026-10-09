use crate::baidu::BaiduClient;
use crate::cloud::{BaiduCloudFs, CloudErrorKind, RemotePath, ResourceKind};
use crate::config::Config;
use crate::filesystem::BaiduFileSystem;
use dav_server::{body::Body, fakels::FakeLs, DavHandler};
use hyper::{
    header::{AUTHORIZATION, WWW_AUTHENTICATE},
    server::conn::http1,
    service::service_fn,
    Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

pub struct ServerHandle {
    shutdown: tokio::sync::oneshot::Sender<()>,
    abort: tokio::sync::watch::Sender<bool>,
    stopped: tokio::sync::oneshot::Receiver<()>,
    addr: std::net::SocketAddr,
}
impl ServerHandle {
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
    pub fn stop(self) {
        let _ = self.shutdown.send(());
    }

    /// Wait until the listening socket has actually been released.
    pub async fn shutdown(self) {
        let _ = self
            .shutdown_with_timeout(std::time::Duration::from_secs(30))
            .await;
    }

    /// Gracefully drain active requests with a bounded timeout.
    pub async fn shutdown_with_timeout(self, timeout: std::time::Duration) -> Result<(), String> {
        let _ = self.shutdown.send(());
        let mut stopped = self.stopped;
        let timed_out = tokio::select! {
            _ = &mut stopped => false,
            _ = tokio::time::sleep(timeout) => true,
        };
        if !timed_out {
            return Ok(());
        }
        let _ = self.abort.send(true);
        match tokio::time::timeout(std::time::Duration::from_secs(2), stopped).await {
            Ok(_) => Err(format!("WebDAV drain 超时，已终止剩余连接：{}", self.addr)),
            Err(_) => Err(format!(
                "WebDAV drain 超时且 abort 清理未完成：{}",
                self.addr
            )),
        }
    }
}

pub async fn start(
    config: Config,
    client: Arc<BaiduClient>,
    temp_dir: PathBuf,
    webdav_password: Option<String>,
) -> Result<ServerHandle, String> {
    config.validate(webdav_password.as_ref().is_some_and(|p| !p.is_empty()))?;
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let cloud = Arc::new(BaiduCloudFs::new(client));
    let fs = BaiduFileSystem::with_cloud(cloud, temp_dir);
    let preflight_fs = fs.clone();
    let dav_prefix = config.dav_prefix.clone();
    let handler = DavHandler::builder()
        .strip_prefix(config.dav_prefix.clone())
        .filesystem(Box::new(fs))
        .locksystem(FakeLs::new())
        .build_handler();
    let auth = if config.basic_auth {
        Some((config.username.clone(), webdav_password.unwrap_or_default()))
    } else {
        None
    };
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let (abort_tx, mut abort_rx) = tokio::sync::watch::channel(false);
    let (stopped_tx, stopped) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        let aborted = loop {
            tokio::select! {
                _ = &mut rx => break false,
                _ = abort_rx.changed() => break true,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break false; };
                    let handler = handler.clone();
                    let auth = auth.clone();
                    let preflight_fs = preflight_fs.clone();
                    let dav_prefix = dav_prefix.clone();
                    connections.spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| {
                            let handler = handler.clone();
                            let auth = auth.clone();
                            let preflight_fs = preflight_fs.clone();
                            let dav_prefix = dav_prefix.clone();
                            async move {
                                if let Some((username, password)) = auth.as_ref() {
                                    if !authorized(&req, username, password) {
                                        let response = Response::builder().status(StatusCode::UNAUTHORIZED).header(WWW_AUTHENTICATE, "Basic realm=\"Pandock WebDAV\"").body(Body::from("WebDAV authentication required")).unwrap();
                                        return Ok::<_, Infallible>(response);
                                    }
                                }
                                if let Some(response) = preflight_parent(&req, &preflight_fs, &dav_prefix).await {
                                    return Ok::<_, Infallible>(response);
                                }
                                Ok::<_, Infallible>(handler.handle(req).await)
                            }
                        });
                        let _ = http1::Builder::new().keep_alive(false).serve_connection(io, service).await;
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        };
        // Dropping the listener before acknowledgement guarantees a rebind can succeed.
        drop(listener);
        if aborted {
            connections.abort_all();
        }
        // Keep listening for abort while waiting for accepted requests to finish.
        while !connections.is_empty() {
            tokio::select! {
                _ = connections.join_next() => {}
                changed = abort_rx.changed() => {
                    if changed.is_ok() && *abort_rx.borrow() {
                        connections.abort_all();
                        break;
                    }
                }
            }
        }
        while connections.join_next().await.is_some() {}
        let _ = stopped_tx.send(());
    });
    Ok(ServerHandle {
        shutdown: tx,
        abort: abort_tx,
        stopped,
        addr,
    })
}

async fn preflight_parent<B>(
    req: &Request<B>,
    fs: &BaiduFileSystem,
    dav_prefix: &str,
) -> Option<Response<Body>> {
    if req.method() != hyper::Method::PUT && req.method().as_str() != "MKCOL" {
        return None;
    }
    let raw_path = req
        .uri()
        .path()
        .strip_prefix(dav_prefix)
        .unwrap_or(req.uri().path());
    let raw_path = if raw_path.is_empty() { "/" } else { raw_path };
    let path = RemotePath::parse(raw_path).ok()?;
    let parent = path.parent()?;
    if parent.is_root() {
        return None;
    }
    match fs.cloud().stat(&parent).await {
        Ok(snapshot) if snapshot.kind == ResourceKind::Collection => None,
        Ok(_) => Some(
            Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::from("WebDAV parent is not a collection"))
                .unwrap(),
        ),
        Err(error) if error.kind == CloudErrorKind::NotFound => Some(
            Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::from("WebDAV parent collection does not exist"))
                .unwrap(),
        ),
        Err(_) => None,
    }
}

fn authorized<B>(req: &Request<B>, username: &str, password: &str) -> bool {
    let Some(value) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some((scheme, encoded)) = value.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Basic") {
        return false;
    }
    let Ok(decoded) =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded.trim())
    else {
        return false;
    };
    constant_time_eq(&decoded, format!("{username}:{password}").as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn webdav_root_is_mapped_to_application_directory() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "list"))
            .and(query_param("dir", "/apps/Demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "list": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000}]
            })))
            .mount(&api).await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let temp = std::env::temp_dir().join(format!("baidupan-test-{}", uuid::Uuid::new_v4()));
        let handle = start(config, client, temp, None).await.unwrap();
        let addr = format!("http://{}", handle.addr());
        let http = reqwest::Client::new();
        let response = http
            .request(
                reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
                format!("{addr}/dav/"),
            )
            .header("Depth", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::MULTI_STATUS);
        let body = response.text().await.unwrap();
        assert!(body.contains("hello.txt"), "{body}");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn webdav_put_waits_for_baidu_upload_before_replying_created() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "list": []
            })))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "precreate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "uploadid": "u-1", "block_list": [0]
            })))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/pcs/file"))
            .and(query_param("method", "locateupload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error_code": 0, "servers": [{"server": api.uri()}]
            })))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/pcs/superfile2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(200))
                    .set_body_json(serde_json::json!({
                        "errno": 0, "md5": "cloud-md5"
                    })),
            )
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "fs_id": 1
            })))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -9,
                "request_id": 1
            })))
            .up_to_n_times(1)
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000}]
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let temp = std::env::temp_dir().join(format!("baidupan-test-{}", uuid::Uuid::new_v4()));
        let handle = start(config, client, temp, None).await.unwrap();
        let url = format!("http://{}/dav/hello.txt", handle.addr());
        let request = tokio::spawn(async move {
            reqwest::Client::new()
                .put(url)
                .body("hello")
                .send()
                .await
                .unwrap()
                .status()
        });
        let mut upload_started = false;
        for _ in 0..500 {
            if api
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path() == "/rest/2.0/pcs/superfile2")
            {
                upload_started = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(upload_started, "upload request did not start");
        handle
            .shutdown_with_timeout(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(request.await.unwrap(), reqwest::StatusCode::CREATED);
        let requests = api.received_requests().await.unwrap();
        assert!(requests.iter().any(|request| {
            request.url.path() == "/rest/2.0/xpan/file"
                && request
                    .url
                    .query()
                    .unwrap_or_default()
                    .contains("method=create")
        }));
    }

    #[tokio::test]
    async fn webdav_get_streams_bytes_from_baidu_dlink() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "list": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000}]
            })))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000,"dlink":format!("{}/download", api.uri())}]
            })))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/download"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 0-4/5")
                    .set_body_bytes(b"hello".to_vec()),
            )
            .mount(&api)
            .await;

        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .get(format!("http://{}/dav/hello.txt", handle.addr()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(&response.bytes().await.unwrap()[..], b"hello");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn webdav_delete_waits_for_synchronous_baidu_filemanager_result() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000}]
            })))
            .up_to_n_times(1)
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -9,
                "request_id": 1
            })))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemanager"))
            .and(query_param("opera", "delete"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"errno": 0, "path": "/apps/Demo/hello.txt"}]
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .delete(format!("http://{}/dav/hello.txt", handle.addr()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn webdav_mkcol_existing_directory_uses_conflict_semantics() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -8,
                "request_id": 1
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .request(
                reqwest::Method::from_bytes(b"MKCOL").unwrap(),
                format!("http://{}/dav/existing/", handle.addr()),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
        let requests = api.received_requests().await.unwrap();
        let create = requests
            .iter()
            .find(|request| {
                request
                    .url
                    .query()
                    .unwrap_or_default()
                    .contains("method=create")
            })
            .expect("MKCOL must reach Baidu create");
        let body = String::from_utf8_lossy(&create.body);
        assert!(
            body.contains("rtype=0"),
            "MKCOL must not auto-rename: {body}"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn webdav_missing_parent_is_not_found_not_server_error() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -9,
                "request_id": 1
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .get(format!(
                "http://{}/dav/legacy/default/manifest.json",
                handle.addr()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn webdav_basic_auth_rejects_missing_credentials() {
        let api = MockServer::start().await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            basic_auth: true,
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), Some("pw".into()))
            .await
            .unwrap();
        let addr = format!("http://{}", handle.addr());
        let http = reqwest::Client::new();
        let denied = http
            .request(reqwest::Method::OPTIONS, format!("{addr}/dav/"))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
        let allowed = http
            .request(reqwest::Method::OPTIONS, format!("{addr}/dav/"))
            .basic_auth("dav", Some("pw"))
            .send()
            .await
            .unwrap();
        assert_eq!(allowed.status(), reqwest::StatusCode::OK);
        handle.shutdown().await;
    }

    #[test]
    fn basic_authorization_accepts_case_insensitive_scheme_and_rejects_bad_password() {
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"dav:pw");
        let request = Request::builder()
            .header(AUTHORIZATION, format!("basic {encoded}"))
            .body(())
            .unwrap();
        assert!(authorized(&request, "dav", "pw"));

        let request = Request::builder()
            .header(AUTHORIZATION, format!("Basic {encoded}"))
            .body(())
            .unwrap();
        assert!(!authorized(&request, "dav", "wrong"));
    }

    #[tokio::test]
    async fn shutdown_releases_listener_before_restarting_same_port() {
        let client = Arc::new(BaiduClient::new(
            "key".into(),
            "secret".into(),
            "Demo".into(),
            "token".into(),
        ));
        let mut config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let first = start(config.clone(), client.clone(), std::env::temp_dir(), None)
            .await
            .unwrap();
        config.listen = first.addr();
        first.shutdown().await;
        let second = start(config, client, std::env::temp_dir(), None)
            .await
            .expect("port must be reusable after awaited shutdown");
        second.shutdown().await;
    }

    #[tokio::test]
    async fn non_loopback_config_without_password_is_rejected_before_binding() {
        let client = Arc::new(BaiduClient::new(
            "key".into(),
            "secret".into(),
            "Demo".into(),
            "token".into(),
        ));
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "0.0.0.0:0".parse().unwrap(),
            basic_auth: true,
            ..Config::default()
        };
        assert!(start(config, client, std::env::temp_dir(), None)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn webdav_mkcol_missing_parent_returns_conflict() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -9,
                "request_id": 1
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .request(
                reqwest::Method::from_bytes(b"MKCOL").unwrap(),
                format!("http://{}/dav/missing/child/", handle.addr()),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        let requests = api.received_requests().await.unwrap();
        assert!(!requests.iter().any(|request| {
            request
                .url
                .query()
                .unwrap_or_default()
                .contains("method=create")
        }));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn webdav_head_exposes_stable_etag_and_length() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{
                    "server_filename": "hello.txt",
                    "path": "/apps/Demo/hello.txt",
                    "fs_id": 7,
                    "size": 5,
                    "isdir": 0,
                    "server_mtime": 1700000000,
                    "md5": "abc"
                }]
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .head(format!("http://{}/dav/hello.txt", handle.addr()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok()),
            Some("\"f-7-5-1700000000-abc\"")
        );
        assert_eq!(
            response
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok()),
            Some("5")
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn webdav_propfind_reports_metadata_properties() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{
                    "server_filename": "hello.txt",
                    "path": "/apps/Demo/hello.txt",
                    "fs_id": 7,
                    "size": 5,
                    "isdir": 0,
                    "server_mtime": 1700000000,
                    "md5": "abc"
                }]
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .request(
                reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
                format!("http://{}/dav/hello.txt", handle.addr()),
            )
            .header("Depth", "0")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::MULTI_STATUS);
        let body = response.text().await.unwrap();
        assert!(body.contains("getetag"), "{body}");
        assert!(
            body.contains("<D:getcontentlength>5</D:getcontentlength>"),
            "{body}"
        );
        handle.shutdown().await;
    }
    #[tokio::test]
    async fn webdav_put_if_match_mismatch_returns_precondition_failed() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{
                    "server_filename": "hello.txt",
                    "path": "/apps/Demo/hello.txt",
                    "fs_id": 7,
                    "size": 5,
                    "isdir": 0,
                    "server_mtime": 1700000000,
                    "md5": "abc"
                }]
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .put(format!("http://{}/dav/hello.txt", handle.addr()))
            .header("If-Match", "\"wrong\"")
            .body("hello")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PRECONDITION_FAILED);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn webdav_invalid_range_returns_range_not_satisfiable() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{
                    "server_filename": "hello.txt",
                    "path": "/apps/Demo/hello.txt",
                    "fs_id": 7,
                    "size": 5,
                    "isdir": 0,
                    "server_mtime": 1700000000,
                    "md5": "abc"
                }]
            })))
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .get(format!("http://{}/dav/hello.txt", handle.addr()))
            .header("Range", "bytes=10-20")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::RANGE_NOT_SATISFIABLE
        );
        handle.shutdown().await;
    }
    #[tokio::test]
    async fn shutdown_waits_for_active_connection_to_close() {
        let client = Arc::new(BaiduClient::new(
            "key".into(),
            "secret".into(),
            "Demo".into(),
            "token".into(),
        ));
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let stream = tokio::net::TcpStream::connect(handle.addr()).await.unwrap();
        let shutdown = tokio::spawn(async move {
            handle
                .shutdown_with_timeout(std::time::Duration::from_secs(1))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!shutdown.is_finished());
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(1), shutdown)
            .await
            .expect("shutdown should finish after connection closes")
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn shutdown_times_out_on_idle_connection() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/xpan/file"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(500))
                    .set_body_json(serde_json::json!({"errno": 0, "list": []})),
            )
            .mount(&api)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&api.uri()),
        );
        let config = Config {
            app_key: "key".into(),
            app_name: "Demo".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let handle = start(config, client, std::env::temp_dir(), None)
            .await
            .unwrap();
        let addr = handle.addr();
        let url = format!("http://{}/dav/", addr);
        let request = tokio::spawn(async move {
            reqwest::Client::new()
                .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url)
                .header("Depth", "1")
                .send()
                .await
        });
        let mut observed = false;
        for _ in 0..50 {
            if !api.received_requests().await.unwrap().is_empty() {
                observed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(observed, "delayed API request was not observed");
        let result = handle
            .shutdown_with_timeout(std::time::Duration::from_millis(100))
            .await;
        assert!(result.is_err());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), request)
            .await
            .expect("aborted request should finish");
        let rebound = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::net::TcpListener::bind(addr),
        )
        .await
        .expect("listener should be released after abort")
        .expect("listener should rebind after abort");
        drop(rebound);
    }
}
