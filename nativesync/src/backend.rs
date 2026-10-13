use pandock_core::cloud::{CloudError, CloudFs, RemotePath, ResourceSnapshot, UploadOutcome};
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
    /// 上传（ADR-112 §3.3 修订：rtype 分流由调用方决定——
    /// local_only 新建 = 0（撞名即失败）；有基线编辑 stat 比对一致后 = 3（安全覆盖））。
    /// -8 经 CloudErrorKind::AlreadyExists 表达。
    fn upload<'a>(
        &'a self,
        path: &'a RemotePath,
        local: &'a std::path::Path,
        size: u64,
        rtype: u32,
    ) -> futures_util::future::BoxFuture<'a, Result<UploadOutcome, CloudError>>;
    /// 改名/移动（opera=move，保 fs_id）。
    fn rename<'a>(
        &'a self,
        from: &'a RemotePath,
        to: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>>;
    /// 删除（filemanager opera=delete，实测进回收站——ADR-112 §3.4）。
    fn remove<'a>(
        &'a self,
        path: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>>;
    /// 创建目录（本地新建目录的上传）；云端已存在（-8）由调用方按「已存在即成功」处理。
    fn create_dir<'a>(
        &'a self,
        path: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>>;
    /// 批量 stat（陈旧 listing 守卫）：list 管广度、stat 管生杀；
    /// 未命中（查无）的路径不在返回里。生产走 filemetas 批量（一次调用）。
    fn stats<'a>(
        &'a self,
        paths: &'a [RemotePath],
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<ResourceSnapshot>, CloudError>>;
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
    fn upload<'a>(
        &'a self,
        path: &'a RemotePath,
        local: &'a std::path::Path,
        size: u64,
        rtype: u32,
    ) -> futures_util::future::BoxFuture<'a, Result<UploadOutcome, CloudError>> {
        self.inner.upload_detailed(path, local, size, rtype)
    }
    fn rename<'a>(
        &'a self,
        from: &'a RemotePath,
        to: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>> {
        self.inner.rename(from, to, false)
    }
    fn remove<'a>(
        &'a self,
        path: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>> {
        self.inner.remove(path)
    }
    fn create_dir<'a>(
        &'a self,
        path: &'a RemotePath,
    ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>> {
        self.inner.create_collection(path)
    }
    fn stats<'a>(
        &'a self,
        paths: &'a [RemotePath],
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<ResourceSnapshot>, CloudError>> {
        self.inner.stats(paths)
    }

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
        // ADR-112 写方向委托：rename/remove 直达 CloudFs；upload 固定 rtype=0
        // （FakeFs 未覆盖 upload_detailed ⇒ 默认 Unsupported，证明走的就是该方法）。
        assert!(backend.rename(&root, &root).await.is_ok());
        assert!(backend.remove(&root).await.is_ok());
        let error = backend
            .upload(&root, std::path::Path::new("x"), 0, 0)
            .await
            .unwrap_err();
        assert_eq!(error.kind, pandock_core::cloud::CloudErrorKind::Unsupported);
        assert!(backend.create_dir(&root).await.is_ok());
    }
}
