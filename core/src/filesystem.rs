use crate::baidu::BaiduClient;
use crate::cloud::{BaiduCloudFs, CloudError, CloudFs, RemotePath, ResourceKind, ResourceSnapshot};
use crate::webdav::status::to_fs_error;
use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::{future::FutureExt, stream};
use std::future;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

#[derive(Clone)]
pub struct BaiduFileSystem {
    cloud: Arc<dyn CloudFs>,
    temp_dir: PathBuf,
}

impl std::fmt::Debug for BaiduFileSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaiduFileSystem")
            .field("temp_dir", &self.temp_dir)
            .finish_non_exhaustive()
    }
}

impl BaiduFileSystem {
    pub fn new(client: Arc<BaiduClient>, temp_dir: PathBuf) -> Self {
        Self {
            cloud: Arc::new(BaiduCloudFs::new(client)),
            temp_dir,
        }
    }

    #[allow(dead_code)]
    pub fn with_cloud(cloud: Arc<dyn CloudFs>, temp_dir: PathBuf) -> Self {
        Self { cloud, temp_dir }
    }

    fn remote_path(path: &DavPath) -> Result<RemotePath, FsError> {
        let text = String::from_utf8(path.as_bytes().to_vec()).map_err(|_| FsError::Forbidden)?;
        RemotePath::parse(&text).map_err(Self::fs_error)
    }

    fn fs_error(error: CloudError) -> FsError {
        if std::env::var_os("PANDOCK_WEBDAV_DEBUG").is_some() {
            eprintln!(
                "[pandock-debug] cloud error kind={:?} errno={:?} status={:?} request_id={:?} message={}",
                error.kind,
                error.raw_errno,
                error.http_status,
                error.request_id,
                error.message
            );
        }
        to_fs_error(error)
    }

    pub fn cloud(&self) -> &Arc<dyn CloudFs> {
        &self.cloud
    }
}

impl DavFileSystem for BaiduFileSystem {
    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let p = Self::remote_path(path)?;
            if p.is_root() {
                return Ok(Box::new(RemoteMeta::dir()) as Box<dyn DavMetaData>);
            }
            let snapshot = self.cloud.stat(&p).await.map_err(Self::fs_error)?;
            Ok(Box::new(RemoteMeta::from_snapshot(&snapshot)) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let p = Self::remote_path(path)?;
            let items = self.cloud.list(&p).await.map_err(Self::fs_error)?;
            let entries: Vec<Box<dyn DavDirEntry>> = items
                .into_iter()
                .map(|snapshot| Box::new(RemoteEntry { snapshot }) as Box<dyn DavDirEntry>)
                .collect();
            Ok(Box::pin(stream::iter(entries.into_iter().map(Ok)))
                as FsStream<Box<dyn DavDirEntry>>)
        }
        .boxed()
    }

    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let remote_path = Self::remote_path(path)?;
            tokio::fs::create_dir_all(&self.temp_dir)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            let temp = self.temp_dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
            let is_write = options.write || options.create || options.truncate || options.append;

            // Partial writes and append are not part of the WebDAV PUT
            // contract accepted by this adapter. Fail closed instead of
            // overwriting a cloud object with a fragment.
            if is_write && (!options.truncate || options.append) {
                return Err(FsError::NotImplemented);
            }

            let snapshot = if is_write {
                let output = tokio::fs::File::create(&temp)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                drop(output);
                None
            } else {
                let snapshot = self
                    .cloud
                    .stat(&remote_path)
                    .await
                    .map_err(Self::fs_error)?;
                if snapshot.kind == ResourceKind::Collection {
                    return Err(FsError::Forbidden);
                }
                Some(snapshot)
            };

            Ok(Box::new(RemoteFile {
                cloud: self.cloud.clone(),
                temp,
                remote_path,
                snapshot,
                write_mode: is_write,
                expected_size: options.size,
                committed: false,
                position: 0,
            }) as Box<dyn DavFile>)
        }
        .boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let path = Self::remote_path(path)?;
            self.cloud
                .create_collection(&path)
                .await
                .map_err(Self::fs_error)
        }
        .boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let path = Self::remote_path(path)?;
            self.cloud.remove(&path).await.map_err(Self::fs_error)
        }
        .boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let path = Self::remote_path(path)?;
            self.cloud.remove(&path).await.map_err(Self::fs_error)
        }
        .boxed()
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let from = Self::remote_path(from)?;
            let to = Self::remote_path(to)?;
            self.cloud
                .rename(&from, &to, true)
                .await
                .map_err(Self::fs_error)
        }
        .boxed()
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let from = Self::remote_path(from)?;
            let to = Self::remote_path(to)?;
            let source = self.cloud.stat(&from).await.map_err(Self::fs_error)?;
            if source.kind == ResourceKind::Collection {
                // Recursive collection COPY is a Phase 3 compatibility
                // feature. Return an explicit 501 instead of pretending the
                // cloud-side copy has completed.
                return Err(FsError::NotImplemented);
            }
            self.cloud
                .copy(&from, &to, true)
                .await
                .map_err(Self::fs_error)
        }
        .boxed()
    }
}

#[derive(Debug, Clone)]
struct RemoteMeta {
    len: u64,
    modified: SystemTime,
    dir: bool,
    etag: Option<String>,
}

impl RemoteMeta {
    fn dir() -> Self {
        Self {
            len: 0,
            modified: UNIX_EPOCH,
            dir: true,
            etag: None,
        }
    }

    fn file(len: u64, modified: SystemTime) -> Self {
        Self {
            len,
            modified,
            dir: false,
            etag: None,
        }
    }

    fn from_snapshot(snapshot: &ResourceSnapshot) -> Self {
        Self {
            len: snapshot.size,
            modified: snapshot.modified,
            dir: snapshot.kind == ResourceKind::Collection,
            etag: snapshot
                .revision
                .as_ref()
                .map(|revision| revision.value.clone()),
        }
    }
}

impl DavMetaData for RemoteMeta {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }

    fn is_dir(&self) -> bool {
        self.dir
    }

    fn etag(&self) -> Option<String> {
        self.etag.clone()
    }
}

#[derive(Debug)]
struct RemoteEntry {
    snapshot: ResourceSnapshot,
}

impl DavDirEntry for RemoteEntry {
    fn name(&self) -> Vec<u8> {
        self.snapshot
            .server_filename
            .clone()
            .or_else(|| self.snapshot.path.file_name().map(str::to_string))
            .unwrap_or_default()
            .into_bytes()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        future::ready(Ok(
            Box::new(RemoteMeta::from_snapshot(&self.snapshot)) as Box<dyn DavMetaData>
        ))
        .boxed()
    }
}

struct RemoteFile {
    cloud: Arc<dyn CloudFs>,
    temp: PathBuf,
    remote_path: RemotePath,
    snapshot: Option<ResourceSnapshot>,
    write_mode: bool,
    expected_size: Option<u64>,
    committed: bool,
    position: u64,
}

impl std::fmt::Debug for RemoteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFile")
            .field("remote_path", &self.remote_path)
            .field("write_mode", &self.write_mode)
            .field("committed", &self.committed)
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl Drop for RemoteFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp);
    }
}

impl DavFile for RemoteFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let snapshot = self.snapshot.clone();
        let position = self.position;
        async move {
            Ok(Box::new(
                snapshot
                    .as_ref()
                    .map(RemoteMeta::from_snapshot)
                    .unwrap_or_else(|| RemoteMeta::file(position, UNIX_EPOCH)),
            ) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        async move {
            let mut bytes = Vec::with_capacity(buf.remaining());
            while buf.has_remaining() {
                let chunk = buf.chunk();
                bytes.extend_from_slice(chunk);
                let n = chunk.len();
                buf.advance(n);
            }
            self.write_bytes(Bytes::from(bytes)).await
        }
        .boxed()
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        async move {
            if !self.write_mode {
                return Err(FsError::Forbidden);
            }
            if self
                .expected_size
                .is_some_and(|expected| self.position.saturating_add(buf.len() as u64) > expected)
            {
                return Err(FsError::TooLarge);
            }
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&self.temp)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            file.seek(SeekFrom::Start(self.position))
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            file.write_all(&buf)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            self.position += buf.len() as u64;
            Ok(())
        }
        .boxed()
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            if self.write_mode {
                let mut file = tokio::fs::OpenOptions::new()
                    .read(true)
                    .open(&self.temp)
                    .await
                    .map_err(|_| FsError::NotFound)?;
                file.seek(SeekFrom::Start(self.position))
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                let mut data = vec![0u8; count];
                let n = file
                    .read(&mut data)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                data.truncate(n);
                self.position += n as u64;
                return Ok(Bytes::from(data));
            }

            let Some(snapshot) = self.snapshot.as_ref() else {
                return Err(FsError::NotFound);
            };
            if self.position >= snapshot.size {
                return Ok(Bytes::new());
            }
            let length = count.min((snapshot.size - self.position) as usize);
            let data = self
                .cloud
                .read_at(&self.remote_path, snapshot, self.position, length)
                .await
                .map_err(BaiduFileSystem::fs_error)?;
            self.position += data.len() as u64;
            Ok(Bytes::from(data))
        }
        .boxed()
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let len = self
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.size)
                .unwrap_or(0);
            let next = match pos {
                SeekFrom::Start(value) => value as i128,
                SeekFrom::Current(value) => self.position as i128 + value as i128,
                SeekFrom::End(value) => len as i128 + value as i128,
            };
            if next < 0 {
                return Err(FsError::Forbidden);
            }
            self.position = next as u64;
            Ok(self.position)
        }
        .boxed()
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        async move {
            if !self.write_mode || self.committed {
                return Ok(());
            }
            let size = tokio::fs::metadata(&self.temp)
                .await
                .map_err(|_| FsError::GeneralFailure)?
                .len();
            if self.expected_size.is_some_and(|expected| expected != size) {
                return Err(FsError::GeneralFailure);
            }
            self.cloud
                .upload(&self.remote_path, &self.temp, size)
                .await
                .map_err(BaiduFileSystem::fs_error)?;

            let mut verified = None;
            for attempt in 0..3u64 {
                match self.cloud.stat(&self.remote_path).await {
                    Ok(snapshot)
                        if snapshot.kind == ResourceKind::File && snapshot.size == size =>
                    {
                        verified = Some(snapshot);
                        break;
                    }
                    Ok(_) => {}
                    Err(error) if error.is_not_found() || error.retryable => {}
                    Err(error) => return Err(BaiduFileSystem::fs_error(error)),
                }
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
                }
            }
            let Some(snapshot) = verified else {
                return Err(FsError::GeneralFailure);
            };
            self.snapshot = Some(snapshot);
            self.committed = true;
            Ok(())
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baidu::BaiduClient;
    use dav_server::davpath::DavPath;
    use dav_server::fs::{DavFileSystem, OpenOptions};
    use wiremock::{
        matchers::{method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn partial_write_is_rejected_instead_of_truncating_cloud_file() {
        let client = Arc::new(BaiduClient::new(
            "key".into(),
            "secret".into(),
            "Demo".into(),
            "token".into(),
        ));
        let fs = BaiduFileSystem::new(client, std::env::temp_dir());
        let path = DavPath::new("/existing.txt").unwrap();
        let options = OpenOptions {
            write: true,
            create: true,
            truncate: false,
            ..OpenOptions::default()
        };
        assert!(matches!(
            fs.open(&path, options).await,
            Err(FsError::NotImplemented)
        ));
    }

    #[tokio::test]
    async fn short_put_does_not_start_a_cloud_upload() {
        let client = Arc::new(BaiduClient::new(
            "key".into(),
            "secret".into(),
            "Demo".into(),
            "token".into(),
        ));
        let fs = BaiduFileSystem::new(
            client,
            std::env::temp_dir().join(format!("baidupan-test-{}", uuid::Uuid::new_v4())),
        );
        let path = DavPath::new("/incomplete.txt").unwrap();
        let options = OpenOptions {
            write: true,
            create: true,
            truncate: true,
            size: Some(5),
            ..OpenOptions::default()
        };
        let mut file = fs.open(&path, options).await.unwrap();
        file.write_bytes(Bytes::from_static(b"hi")).await.unwrap();
        assert!(file.flush().await.is_err());
    }

    #[tokio::test]
    async fn remote_file_reads_through_baidu_range_download() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000,"dlink":format!("{}/download", server.uri())}]
            })))
            .mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/download"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;

        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&server.uri()),
        );
        let fs = BaiduFileSystem::new(
            client,
            std::env::temp_dir().join(format!("baidupan-test-{}", uuid::Uuid::new_v4())),
        );
        let path = DavPath::new("/hello.txt").unwrap();
        let options = OpenOptions {
            read: true,
            ..Default::default()
        };
        let mut file = fs.open(&path, options).await.unwrap();
        let data = file.read_bytes(5).await.unwrap();
        assert_eq!(&data[..], b"hello");
    }

    #[tokio::test]
    async fn remote_file_flush_uploads_through_baidu_three_phase_flow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "precreate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"errno":0,"uploadid":"upload-1","block_list":[0]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/pcs/file"))
            .and(query_param("method", "locateupload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"error_code":0,"servers":[{"server":server.uri()}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/pcs/superfile2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"errno":0,"md5":"cloud-md5"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/rest/2.0/xpan/file")).and(query_param("method", "create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"errno":0,"fs_id":1,"path":"/apps/Demo/hello.txt","size":5,"isdir":0})))
            .mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000}]
            })))
            .mount(&server)
            .await;
        let client = Arc::new(
            BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
                .with_base_urls(&server.uri()),
        );
        let temp = std::env::temp_dir().join(format!("baidupan-test-{}", uuid::Uuid::new_v4()));
        let fs = BaiduFileSystem::new(client, temp);
        let path = DavPath::new("/hello.txt").unwrap();
        let options = OpenOptions {
            write: true,
            create: true,
            truncate: true,
            ..Default::default()
        };
        let mut file = fs.open(&path, options).await.unwrap();
        file.write_bytes(Bytes::from_static(b"hello"))
            .await
            .unwrap();
        file.flush().await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert!(!requests.iter().any(|request| {
            request.url.path() == "/rest/2.0/xpan/file"
                && request
                    .url
                    .query()
                    .unwrap_or_default()
                    .contains("method=meta")
        }));
        let create = requests
            .iter()
            .find(|request| {
                request.url.as_str().contains("method=create")
                    && request.url.as_str().contains("xpan/file")
            })
            .expect("create request");
        let body = String::from_utf8_lossy(&create.body);
        assert!(
            body.contains("cloud-md5"),
            "create body did not use cloud md5: {body}"
        );
    }
}
