use crate::backend::CloudBackend;
use md5::Context;
use pandock_core::cloud::{RemotePath, ResourceSnapshot};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub const HYDRATION_CHUNK_SIZE: usize = 4 * 1024 * 1024;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HydrationError {
    #[error("hydration cancelled")]
    Cancelled,
    #[error("short read: {requested}/{received}")]
    ShortRead { requested: usize, received: usize },
    #[error("empty remote read")]
    EmptyRead,
    #[error("size mismatch: {expected}/{received}")]
    SizeMismatch { expected: u64, received: u64 },
    #[error("md5 mismatch")]
    Md5Mismatch,
    #[error("backend: {0}")]
    Backend(String),
    #[error("transfer: {0}")]
    Transfer(String),
}
pub struct HydrationRequest {
    pub file_id: u64,
    pub remote: RemotePath,
    pub snapshot: ResourceSnapshot,
    pub offset: u64,
    pub length: i64,
    pub cancelled: Arc<AtomicBool>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationResult {
    pub file_id: u64,
    pub bytes: u64,
    pub full_file: bool,
}
pub async fn hydrate_range<F>(
    backend: Arc<dyn CloudBackend>,
    req: HydrationRequest,
    mut transfer: F,
) -> Result<HydrationResult, HydrationError>
where
    F: FnMut(u64, &[u8]) -> Result<(), HydrationError>,
{
    let total = req.snapshot.size;
    let start = req.offset;
    let requested = if req.length < 0 {
        total.saturating_sub(start)
    } else {
        (req.length as u64).min(total.saturating_sub(start))
    };
    let full = start == 0 && requested == total;
    let mut remain = requested;
    let mut offset = start;
    let mut done = 0;
    let mut digest = Context::new();
    while remain > 0 {
        if req.cancelled.load(Ordering::SeqCst) {
            return Err(HydrationError::Cancelled);
        };
        let want = remain.min(HYDRATION_CHUNK_SIZE as u64) as usize;
        let read = backend.read_at(&req.remote, &req.snapshot, offset, want);
        tokio::pin!(read);
        let bytes = loop {
            tokio::select! {
                result = &mut read => {
                    break result.map_err(|e| HydrationError::Backend(format!("{:?}", e.kind)))?;
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                    if req.cancelled.load(Ordering::SeqCst) {
                        return Err(HydrationError::Cancelled);
                    }
                }
            }
        };
        if bytes.is_empty() {
            return Err(HydrationError::EmptyRead);
        };
        if bytes.len() < want && remain > bytes.len() as u64 {
            return Err(HydrationError::ShortRead {
                requested: want,
                received: bytes.len(),
            });
        };
        if full {
            digest.consume(&bytes)
        };
        transfer(offset, &bytes)?;
        offset += bytes.len() as u64;
        done += bytes.len() as u64;
        remain -= bytes.len() as u64;
    }
    if full {
        if done != total {
            return Err(HydrationError::SizeMismatch {
                expected: total,
                received: done,
            });
        };
        // 百度 md5 字段不保证是 32 位小写十六进制（历史数据可能是其它编码），
        // 非十六进制值不参与比对，避免误报（P0 复盘：真实文件被误判 md5 mismatch）。
        if let Some(expected) = req.snapshot.md5.as_deref() {
            let hex = expected.len() == 32 && expected.bytes().all(|b| b.is_ascii_hexdigit());
            if hex && format!("{:x}", digest.compute()) != expected.to_ascii_lowercase() {
                return Err(HydrationError::Md5Mismatch);
            }
        }
    };
    Ok(HydrationResult {
        file_id: req.file_id,
        bytes: done,
        full_file: full,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::CloudBackend;
    use futures_util::FutureExt;
    use pandock_core::cloud::{CloudError, ResourceSnapshot};
    use std::sync::Mutex;
    struct Fake {
        data: Vec<u8>,
        fail: bool,
        short: bool,
        delay_ms: u64,
        calls: Mutex<Vec<(u64, usize)>>,
    }
    impl CloudBackend for Fake {
        fn stat<'a>(
            &'a self,
            _: &'a RemotePath,
        ) -> futures_util::future::BoxFuture<'a, Result<ResourceSnapshot, CloudError>> {
            async move {
                Ok(ResourceSnapshot::from_entry(
                    RemotePath::parse("/f").unwrap(),
                    Some(1),
                    self.data.len() as u64,
                    0,
                    Some(1),
                    Some(format!("{:x}", md5::compute(&self.data))),
                    Some("f".into()),
                    None,
                    None,
                ))
            }
            .boxed()
        }
        fn list<'a>(
            &'a self,
            _: &'a RemotePath,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<ResourceSnapshot>, CloudError>>
        {
            async move { Ok(vec![]) }.boxed()
        }
        fn read_at<'a>(
            &'a self,
            _: &'a RemotePath,
            _: &'a ResourceSnapshot,
            start: u64,
            count: usize,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>, CloudError>> {
            async move {
                self.calls.lock().unwrap().push((start, count));
                if self.delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
                }
                if self.fail {
                    return Err(CloudError::unknown("fake"));
                };
                let end = (start as usize + count).min(self.data.len());
                let mut out = self.data[start as usize..end].to_vec();
                if self.short && out.len() > 1 {
                    out.pop();
                }
                Ok(out)
            }
            .boxed()
        }
    }
    fn req(data: &[u8], len: i64, cancel: Arc<AtomicBool>) -> HydrationRequest {
        HydrationRequest {
            file_id: 1,
            remote: RemotePath::parse("/f").unwrap(),
            snapshot: ResourceSnapshot::from_entry(
                RemotePath::parse("/f").unwrap(),
                Some(1),
                data.len() as u64,
                0,
                Some(1),
                Some(format!("{:x}", md5::compute(data))),
                Some("f".into()),
                None,
                None,
            ),
            offset: 0,
            length: len,
            cancelled: cancel,
        }
    }
    #[tokio::test]
    async fn full_read_chunks_and_checks_md5() {
        let data = vec![7u8; HYDRATION_CHUNK_SIZE + 19];
        let fake = Arc::new(Fake {
            data: data.clone(),
            fail: false,
            short: false,
            delay_ms: 0,
            calls: Mutex::new(vec![]),
        });
        let mut out = Vec::new();
        let r = hydrate_range(
            fake,
            req(&data, -1, Arc::new(AtomicBool::new(false))),
            |_, b| {
                out.extend_from_slice(b);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(r.full_file);
        assert_eq!(out, data);
    }
    #[tokio::test]
    async fn short_read_and_backend_error_are_rejected() {
        let data = vec![1u8; 99];
        let fake = Arc::new(Fake {
            data: data.clone(),
            fail: false,
            short: true,
            delay_ms: 0,
            calls: Mutex::new(vec![]),
        });
        assert!(matches!(
            hydrate_range(
                fake,
                req(&data, -1, Arc::new(AtomicBool::new(false))),
                |_, _| Ok(())
            )
            .await,
            Err(HydrationError::ShortRead { .. })
        ));
        let fake = Arc::new(Fake {
            data: data.clone(),
            fail: true,
            short: false,
            delay_ms: 0,
            calls: Mutex::new(vec![]),
        });
        assert!(matches!(
            hydrate_range(
                fake,
                req(&data, -1, Arc::new(AtomicBool::new(false))),
                |_, _| Ok(())
            )
            .await,
            Err(HydrationError::Backend(_))
        ));
    }
    #[tokio::test]
    async fn cancellation_stops_before_read() {
        let data = vec![1u8; 99];
        let cancel = Arc::new(AtomicBool::new(true));
        let fake = Arc::new(Fake {
            data,
            fail: false,
            short: false,
            delay_ms: 0,
            calls: Mutex::new(vec![]),
        });
        assert_eq!(
            hydrate_range(fake, req(&[1u8; 99], -1, cancel), |_, _| Ok(())).await,
            Err(HydrationError::Cancelled)
        );
    }
    #[tokio::test]
    async fn cancellation_during_backend_read_is_prompt() {
        let data = vec![3u8; 128];
        let cancel = Arc::new(AtomicBool::new(false));
        let fake = Arc::new(Fake {
            data: data.clone(),
            fail: false,
            short: false,
            delay_ms: 500,
            calls: Mutex::new(vec![]),
        });
        let task_cancel = cancel.clone();
        let task = tokio::spawn(hydrate_range(fake, req(&data, -1, cancel), |_, _| Ok(())));
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        task_cancel.store(true, Ordering::SeqCst);
        assert_eq!(task.await.unwrap(), Err(HydrationError::Cancelled));
    }

    #[tokio::test]
    async fn non_hex_baidu_md5_is_skipped_and_wrong_hex_md5_fails() {
        let data = vec![9u8; 64];
        let mut request = req(&data, -1, Arc::new(AtomicBool::new(false)));
        request.snapshot.md5 = Some("de6c91ae7h0458b98db00d5cc1ca50a7".into());
        let fake = Arc::new(Fake {
            data: data.clone(),
            fail: false,
            short: false,
            delay_ms: 0,
            calls: Mutex::new(vec![]),
        });
        let r = hydrate_range(fake, request, |_, _| Ok(())).await.unwrap();
        assert!(r.full_file);
        let mut request = req(&data, -1, Arc::new(AtomicBool::new(false)));
        request.snapshot.md5 = Some(format!("{:032x}", 0u128));
        let fake = Arc::new(Fake {
            data,
            fail: false,
            short: false,
            delay_ms: 0,
            calls: Mutex::new(vec![]),
        });
        assert!(matches!(
            hydrate_range(fake, request, |_, _| Ok(())).await,
            Err(HydrationError::Md5Mismatch)
        ));
    }
    #[tokio::test]
    async fn partial_range_is_not_full() {
        let data = vec![2u8; 100];
        let fake = Arc::new(Fake {
            data,
            fail: false,
            short: false,
            delay_ms: 0,
            calls: Mutex::new(vec![]),
        });
        let r = hydrate_range(
            fake,
            req(&[2u8; 100], 20, Arc::new(AtomicBool::new(false))),
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        assert!(!r.full_file);
        assert_eq!(r.bytes, 20);
    }
}
