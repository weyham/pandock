use pandock_core::cloud::{CloudError, CloudFs, RemotePath, ResourceSnapshot};
use std::sync::Arc;

/// Read-only backend contract used by the sync manager. No WebDAV calls are
/// made here; the manager talks directly to the Baidu-backed CloudFs.
pub trait CloudBackend: Send + Sync {
    fn stat<'a>(
        &'a self,
        path: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<ResourceSnapshot, CloudError>>;
    fn list<'a>(
        &'a self,
        path: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<ResourceSnapshot>, CloudError>>;
    fn read_at<'a>(
        &'a self,
        path: &'a RemotePath,
        snapshot: &'a ResourceSnapshot,
        start: u64,
        count: usize,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>, CloudError>>;
}

pub struct SnapshotBackend {
    inner: Arc<dyn CloudFs>,
}

impl SnapshotBackend {
    pub fn new(inner: Arc<dyn CloudFs>) -> Self {
        Self { inner }
    }
}

impl CloudBackend for SnapshotBackend {
    fn stat<'a>(
        &'a self,
        path: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<ResourceSnapshot, CloudError>> {
        self.inner.stat(path)
    }
    fn list<'a>(
        &'a self,
        path: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<ResourceSnapshot>, CloudError>> {
        self.inner.list(path)
    }
    fn read_at<'a>(
        &'a self,
        path: &'a RemotePath,
        snapshot: &'a ResourceSnapshot,
        start: u64,
        count: usize,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>, CloudError>> {
        self.inner.read_at(path, snapshot, start, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use pandock_core::cloud::{CloudFuture, ResourceSnapshot};

    struct FakeFs;
    impl CloudFs for FakeFs {
        fn stat<'a>(&'a self, _path: &'a RemotePath) -> CloudFuture<'a, ResourceSnapshot> {
            async move {
                Ok(ResourceSnapshot::synthetic_dir(
                    RemotePath::parse("/x").unwrap(),
                ))
            }
            .boxed()
        }
        fn list<'a>(&'a self, _path: &'a RemotePath) -> CloudFuture<'a, Vec<ResourceSnapshot>> {
            async move { Ok(vec![]) }.boxed()
        }
        fn create_collection<'a>(&'a self, _path: &'a RemotePath) -> CloudFuture<'a, ()> {
            async move { Ok(()) }.boxed()
        }
        fn remove<'a>(&'a self, _path: &'a RemotePath) -> CloudFuture<'a, ()> {
            async move { Ok(()) }.boxed()
        }
        fn rename<'a>(
            &'a self,
            _from: &'a RemotePath,
            _to: &'a RemotePath,
            _overwrite: bool,
        ) -> CloudFuture<'a, ()> {
            async move { Ok(()) }.boxed()
        }
        fn copy<'a>(
            &'a self,
            _from: &'a RemotePath,
            _to: &'a RemotePath,
            _overwrite: bool,
        ) -> CloudFuture<'a, ()> {
            async move { Ok(()) }.boxed()
        }
        fn upload<'a>(
            &'a self,
            _path: &'a RemotePath,
            _local: &'a std::path::Path,
            _size: u64,
        ) -> CloudFuture<'a, ()> {
            async move { Ok(()) }.boxed()
        }
        fn read_at<'a>(
            &'a self,
            _path: &'a RemotePath,
            _snapshot: &'a ResourceSnapshot,
            _start: u64,
            _count: usize,
        ) -> CloudFuture<'a, Vec<u8>> {
            async move { Ok(vec![1, 2, 3]) }.boxed()
        }
    }

    #[tokio::test]
    async fn snapshot_backend_delegates_readonly_calls() {
        let backend = SnapshotBackend::new(Arc::new(FakeFs));
        let root = RemotePath::parse("/").unwrap();
        let snap = backend.stat(&root).await.unwrap();
        assert!(snap.is_dir());
        assert!(backend.list(&root).await.unwrap().is_empty());
        let bytes = backend
            .read_at(&RemotePath::parse("/f").unwrap(), &snap, 0, 3)
            .await
            .unwrap();
        assert_eq!(bytes, vec![1, 2, 3]);
    }
}
