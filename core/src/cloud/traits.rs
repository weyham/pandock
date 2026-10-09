use std::path::Path;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;

use super::error::CloudError;
use super::resource::{RemotePath, ResourceKind, ResourceSnapshot};

pub type CloudFuture<'a, T> = BoxFuture<'a, Result<T, CloudError>>;

pub trait CloudFs: Send + Sync {
    fn stat<'a>(&'a self, path: &'a RemotePath) -> CloudFuture<'a, ResourceSnapshot>;
    fn list<'a>(&'a self, path: &'a RemotePath) -> CloudFuture<'a, Vec<ResourceSnapshot>>;
    fn create_collection<'a>(&'a self, path: &'a RemotePath) -> CloudFuture<'a, ()>;
    fn remove<'a>(&'a self, path: &'a RemotePath) -> CloudFuture<'a, ()>;
    fn rename<'a>(
        &'a self,
        from: &'a RemotePath,
        to: &'a RemotePath,
        overwrite: bool,
    ) -> CloudFuture<'a, ()>;
    fn copy<'a>(
        &'a self,
        from: &'a RemotePath,
        to: &'a RemotePath,
        overwrite: bool,
    ) -> CloudFuture<'a, ()>;
    fn upload<'a>(
        &'a self,
        path: &'a RemotePath,
        local: &'a Path,
        size: u64,
    ) -> CloudFuture<'a, ()>;
    fn read_at<'a>(
        &'a self,
        path: &'a RemotePath,
        snapshot: &'a ResourceSnapshot,
        start: u64,
        count: usize,
    ) -> CloudFuture<'a, Vec<u8>>;
}

#[derive(Clone)]
pub struct BaiduCloudFs {
    client: std::sync::Arc<crate::baidu::BaiduClient>,
}

impl BaiduCloudFs {
    pub fn new(client: std::sync::Arc<crate::baidu::BaiduClient>) -> Self {
        Self { client }
    }

    fn convert_entry(&self, path: RemotePath, entry: crate::baidu::FileEntry) -> ResourceSnapshot {
        ResourceSnapshot::from_entry(
            path,
            Some(entry.fs_id).filter(|value| *value != 0),
            entry.size,
            entry.isdir,
            entry.server_mtime,
            entry.md5.clone(),
            Some(entry.server_filename).filter(|value| !value.is_empty()),
            entry.real_category.clone(),
            entry.dlink.clone(),
        )
    }

    async fn expect_exists(&self, path: &RemotePath) -> Result<ResourceSnapshot, CloudError> {
        let mut last_error = None;
        for attempt in 0..3u64 {
            match self.client.meta(path.as_str()).await {
                Ok(entry) => return Ok(self.convert_entry(path.clone(), entry)),
                Err(message) => {
                    let error = CloudError::from_message(message);
                    if error.is_not_found() && attempt < 2 {
                        last_error = Some(error);
                        tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1)))
                            .await;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| CloudError::not_found("写后验证失败：资源不存在")))
    }

    async fn expect_missing(&self, path: &RemotePath) -> Result<(), CloudError> {
        let mut last_error = None;
        for attempt in 0..3u64 {
            match self.client.meta(path.as_str()).await {
                Err(message) => {
                    let error = CloudError::from_message(message);
                    if error.is_not_found() {
                        return Ok(());
                    }
                    last_error = Some(error);
                }
                Ok(_) => {
                    last_error = Some(CloudError::unknown("写后验证失败：资源仍然存在"));
                }
            }
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
            }
        }
        Err(last_error.unwrap_or_else(|| CloudError::unknown("写后验证失败")))
    }
}

impl CloudFs for BaiduCloudFs {
    fn stat<'a>(&'a self, path: &'a RemotePath) -> CloudFuture<'a, ResourceSnapshot> {
        async move {
            let entry = self
                .client
                .meta(path.as_str())
                .await
                .map_err(CloudError::from_message)?;
            Ok(self.convert_entry(path.clone(), entry))
        }
        .boxed()
    }

    fn list<'a>(&'a self, path: &'a RemotePath) -> CloudFuture<'a, Vec<ResourceSnapshot>> {
        async move {
            let entries = self
                .client
                .list(path.as_str())
                .await
                .map_err(CloudError::from_message)?;
            entries
                .into_iter()
                .map(|entry| {
                    let child = match path.file_name() {
                        Some(_) => {
                            let name = entry.server_filename.clone();
                            path.join(&name)
                        }
                        None => RemotePath::parse(&format!("/{}", entry.server_filename)),
                    }?;
                    Ok(self.convert_entry(child, entry))
                })
                .collect()
        }
        .boxed()
    }

    fn create_collection<'a>(&'a self, path: &'a RemotePath) -> CloudFuture<'a, ()> {
        async move {
            match self.client.mkdir(path.as_str()).await {
                Ok(()) => {
                    let snapshot = self.expect_exists(path).await?;
                    if snapshot.kind == ResourceKind::Collection {
                        Ok(())
                    } else {
                        Err(CloudError::unknown("MKCOL 后资源不是目录"))
                    }
                }
                Err(message) => {
                    let error = CloudError::from_message(message);
                    if error.raw_errno == Some(-9) {
                        Err(CloudError::parent_missing("父目录不存在"))
                    } else {
                        Err(error)
                    }
                }
            }
        }
        .boxed()
    }

    fn remove<'a>(&'a self, path: &'a RemotePath) -> CloudFuture<'a, ()> {
        async move {
            self.client
                .delete(path.as_str())
                .await
                .map_err(CloudError::from_message)?;
            self.expect_missing(path).await
        }
        .boxed()
    }

    fn rename<'a>(
        &'a self,
        from: &'a RemotePath,
        to: &'a RemotePath,
        overwrite: bool,
    ) -> CloudFuture<'a, ()> {
        async move {
            self.client
                .move_to_with_overwrite(from.as_str(), to.as_str(), overwrite)
                .await
                .map_err(CloudError::from_message)?;
            let _ = self.expect_exists(to).await?;
            self.expect_missing(from).await
        }
        .boxed()
    }

    fn copy<'a>(
        &'a self,
        from: &'a RemotePath,
        to: &'a RemotePath,
        overwrite: bool,
    ) -> CloudFuture<'a, ()> {
        async move {
            self.client
                .copy_to_with_overwrite(from.as_str(), to.as_str(), overwrite)
                .await
                .map_err(CloudError::from_message)?;
            let _ = self.expect_exists(to).await?;
            Ok(())
        }
        .boxed()
    }

    fn upload<'a>(
        &'a self,
        path: &'a RemotePath,
        local: &'a Path,
        size: u64,
    ) -> CloudFuture<'a, ()> {
        async move {
            self.client
                .upload_file(path.as_str(), local, size)
                .await
                .map_err(CloudError::from_message)
        }
        .boxed()
    }

    fn read_at<'a>(
        &'a self,
        _path: &'a RemotePath,
        snapshot: &'a ResourceSnapshot,
        start: u64,
        count: usize,
    ) -> CloudFuture<'a, Vec<u8>> {
        async move {
            let dlink = snapshot
                .dlink
                .as_deref()
                .ok_or_else(|| CloudError::unknown("百度未返回下载直链"))?;
            self.client
                .download_range(dlink, start, count)
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(CloudError::from_message)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baidu::BaiduClient;
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> Arc<BaiduClient> {
        Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&server.uri()),
        )
    }

    #[tokio::test]
    async fn rename_verifies_destination_and_source_removal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemanager"))
            .and(query_param("opera", "move"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"errno": 0, "path": "/apps/Demo/b.txt"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .and(body_string_contains("b.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"server_filename":"b.txt","path":"/apps/Demo/b.txt","fs_id":2,"size":5,"isdir":0,"server_mtime":1700000001}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .and(body_string_contains("a.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -9,
                "request_id": 1
            })))
            .mount(&server)
            .await;

        let fs = BaiduCloudFs::new(client(&server));
        fs.rename(
            &RemotePath::parse("/a.txt").unwrap(),
            &RemotePath::parse("/b.txt").unwrap(),
            true,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn copy_verifies_destination() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemanager"))
            .and(query_param("opera", "copy"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"errno": 0, "path": "/apps/Demo/b.txt"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"server_filename":"b.txt","path":"/apps/Demo/b.txt","fs_id":2,"size":5,"isdir":0,"server_mtime":1700000001}]
            })))
            .mount(&server)
            .await;

        let fs = BaiduCloudFs::new(client(&server));
        fs.copy(
            &RemotePath::parse("/a.txt").unwrap(),
            &RemotePath::parse("/b.txt").unwrap(),
            true,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn delete_verifies_missing_resource() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemanager"))
            .and(query_param("opera", "delete"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"errno": 0, "path": "/apps/Demo/a.txt"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -9,
                "request_id": 1
            })))
            .mount(&server)
            .await;

        let fs = BaiduCloudFs::new(client(&server));
        fs.remove(&RemotePath::parse("/a.txt").unwrap())
            .await
            .unwrap();
    }
}
