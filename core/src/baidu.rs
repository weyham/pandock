use crate::cloud::UploadOutcome;
use reqwest::{multipart, Client};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const PAN_API: &str = "https://pan.baidu.com";
const OPEN_API: &str = "https://openapi.baidu.com";
const UPLOAD_API: &str = "https://d.pcs.baidu.com";
const CHUNK_SIZE: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct BaiduClient {
    http: Client,
    pub app_key: String,
    pub app_secret: String,
    pub app_name: String,
    pub token: String,
    pub pan_api: String,
    pub open_api: String,
    pub upload_api: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub scope: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: Option<String>,
    pub verification_url: Option<String>,
    pub qrcode_url: Option<String>,
    pub expires_in: u64,
    pub interval: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileEntry {
    pub server_filename: String,
    pub path: String,
    pub fs_id: u64,
    pub size: u64,
    pub isdir: u8,
    pub server_mtime: Option<i64>,
    pub local_mtime: Option<i64>,
    pub md5: Option<String>,
    pub real_category: Option<String>,
    pub dlink: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListResponse {
    pub list: Vec<FileEntry>,
}

/// `upload_precreate_and_parts` 的产物：待 create 的全部材料。
struct UploadPrepared {
    path: String,
    upload_id: String,
    block_list: String,
}

/// `method=list` 的固定分页大小。NativeSync 自动对账按它把一次目录列举折算成实际 HTTP
/// 调用数（`count / LIST_PAGE_SIZE + 1`），计入每层配额预算——改动它必须同步 nativesync 的折算逻辑。
pub const LIST_PAGE_SIZE: usize = 1000;

impl BaiduClient {
    pub fn new(app_key: String, app_secret: String, app_name: String, token: String) -> Self {
        Self {
            http: Client::builder()
                .user_agent("pan.baidu.com")
                .build()
                .expect("reqwest client must build"),
            app_key: app_key.trim().to_string(),
            app_secret: app_secret.trim().to_string(),
            app_name: app_name.trim().to_string(),
            token,
            pan_api: PAN_API.to_string(),
            open_api: OPEN_API.to_string(),
            upload_api: UPLOAD_API.to_string(),
        }
    }

    pub fn with_base_urls(mut self, base: &str) -> Self {
        self.pan_api = base.to_string();
        self.open_api = base.to_string();
        self.upload_api = base.to_string();
        self
    }

    pub fn remote_path(&self, dav_path: &str) -> Result<String, String> {
        let path = dav_path.trim_matches('/');
        if path.split('/').any(|part| part == ".." || part.is_empty()) && !path.is_empty() {
            return Err("非法远端路径".into());
        }
        if path.is_empty() {
            Ok(format!("/apps/{}", self.app_name))
        } else {
            Ok(format!("/apps/{}/{}", self.app_name, path))
        }
    }

    fn require_child_path(&self, dav_path: &str) -> Result<(), String> {
        if dav_path.trim_matches('/').is_empty() {
            return Err("不能对 WebDAV 根目录执行写操作".into());
        }
        self.remote_path(dav_path).map(|_| ())
    }

    /// Ensure the application directory exists before exposing it as the
    /// WebDAV root. The open-platform app directory is not guaranteed to be
    /// created by Baidu when a user first authorizes the application.
    pub async fn ensure_app_root(&self) -> Result<(), String> {
        match self.list("/").await {
            Ok(_) => return Ok(()),
            Err(error) if api_error_code(&error) == Some(-9) => {}
            Err(error) => return Err(error),
        }

        let path = self.remote_path("/")?;
        let response = self
            .api_post("/rest/2.0/xpan/file")
            .query(&[("method", "create")])
            .form(&[
                ("path", path.as_str()),
                ("isdir", "1"),
                // Do not silently create a second application folder if a
                // concurrent request or eventual consistency reports a race.
                ("rtype", "0"),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;

        match self.ensure_success(response).await {
            Ok(()) => Ok(()),
            Err(error) if api_error_code(&error) == Some(-8) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub async fn device_code(&self) -> Result<DeviceCodeResponse, String> {
        let value = self
            .http
            .get(format!("{}/oauth/2.0/device/code", self.open_api))
            .query(&[
                ("response_type", "device_code"),
                ("client_id", &self.app_key),
                ("scope", "basic,netdisk"),
            ])
            .send()
            .await
            .map_err(|_| "百度授权服务暂时不可达".to_string())?
            .error_for_status()
            .map_err(|e| {
                format!(
                    "设备码请求失败: HTTP {}",
                    e.status()
                        .map(|s| s.as_u16().to_string())
                        .unwrap_or_else(|| "未知".into())
                )
            })?
            .json::<DeviceCodeResponse>()
            .await
            .map_err(|_| "设备码响应格式错误".to_string())?;
        Ok(value)
    }

    pub async fn poll_device_token(&self, device_code: &str) -> Result<TokenResponse, String> {
        let response = self
            .http
            .get(format!("{}/oauth/2.0/token", self.open_api))
            .query(&[
                ("grant_type", "device_token"),
                ("code", device_code),
                ("client_id", &self.app_key),
                ("client_secret", &self.app_secret),
            ])
            .send()
            .await
            .map_err(|_| "百度授权服务暂时不可达".to_string())?;
        let status = response.status();
        let data: serde_json::Value = response
            .json()
            .await
            .map_err(|_| format!("设备码响应格式错误：HTTP {status}"))?;
        if let Some(error) = data.get("error").and_then(|value| value.as_str()) {
            return match error {
                "authorization_pending" => Err("PENDING".into()),
                "slow_down" => Err("SLOW_DOWN".into()),
                "invalid_client" => Err(
                    "invalid_client：App Key 与 Secret Key 不匹配，请确认二者来自同一个已生效应用，不要误填 Signkey"
                        .into(),
                ),
                _ => Err(format!("设备码授权失败: {error}")),
            };
        }
        if !status.is_success() {
            return Err(format!("设备码轮询失败: HTTP {status}"));
        }
        serde_json::from_value::<TokenResponse>(data).map_err(|e| e.to_string())
    }

    pub async fn refresh_token(&self, refresh_token: &str) -> Result<TokenResponse, String> {
        let response = self
            .http
            .get(format!("{}/oauth/2.0/token", self.open_api))
            .query(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &self.app_key),
                ("client_secret", &self.app_secret),
            ])
            .send()
            .await
            // reqwest's Display includes the full query string. Never expose
            // client_secret/refresh_token through a UI error.
            .map_err(|_| "百度授权服务暂时不可达".to_string())?;
        let status = response.status();
        let data: serde_json::Value = response
            .json()
            .await
            .map_err(|_| format!("刷新百度授权失败：HTTP {status}"))?;
        if let Some(error) = data.get("error").and_then(|value| value.as_str()) {
            return match error {
                "invalid_grant" => Err("百度授权已失效，请重新连接百度网盘".into()),
                "invalid_client" => Err(
                    "invalid_client：App Key 与 Secret Key 不匹配，请确认二者来自同一个已生效应用"
                        .into(),
                ),
                _ => Err(format!("刷新百度授权失败: {error}")),
            };
        }
        if !status.is_success() {
            return Err(format!("刷新百度授权失败: HTTP {status}"));
        }
        serde_json::from_value(data).map_err(|_| "刷新百度授权响应格式错误".into())
    }

    pub async fn list(&self, dav_path: &str) -> Result<Vec<FileEntry>, String> {
        let dir = self.remote_path(dav_path)?;
        let mut all = Vec::new();
        let mut start = 0usize;
        const PAGE_SIZE: usize = LIST_PAGE_SIZE;
        loop {
            let query = vec![
                ("method", "list".to_string()),
                ("dir", dir.clone()),
                ("start", start.to_string()),
                ("limit", PAGE_SIZE.to_string()),
                ("web", "1".to_string()),
                ("order", "name".to_string()),
            ];
            let response = self
                .api_get("/rest/2.0/xpan/file")
                .query(&query)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let data = self.decode_json(response).await?;
            let value: ListResponse = serde_json::from_value(data).map_err(|e| e.to_string())?;
            let count = value.list.len();
            all.extend(value.list);
            if count < PAGE_SIZE {
                break;
            }
            start += count;
        }
        Ok(all)
    }

    /// 批量 filemetas（target=[路径数组]，生产 meta 同形态）——
    /// 陈旧 listing 守卫的确认通道：list 管广度、stat 管生杀（2026-10-13 实测 meta 是新鲜的）。
    /// 返回与输入同序的命中项（每项含 `path` 字段可归属）；未命中的路径**不在返回里**。
    pub async fn metas(&self, dav_paths: &[String]) -> Result<Vec<FileEntry>, String> {
        if dav_paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::with_capacity(dav_paths.len());
        for dav_path in dav_paths {
            paths.push(self.remote_path(dav_path)?);
        }
        let target = serde_json::to_string(&paths).map_err(|e| e.to_string())?;
        let response = self
            .api_post("/rest/2.0/xpan/file")
            .query(&[("method", "filemetas")])
            .form(&[("target", target.as_str()), ("dlink", "0")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let data = self.decode_json(response).await?;
        let entries: Vec<FileEntry> = serde_json::from_value(
            data.get("info")
                .or_else(|| data.get("list"))
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )
        .map_err(|e| e.to_string())?;
        Ok(entries)
    }

    pub async fn meta(&self, dav_path: &str) -> Result<FileEntry, String> {
        let path = self.remote_path(dav_path)?;
        let target = serde_json::to_string(&[path.as_str()]).map_err(|e| e.to_string())?;
        let response = self
            .api_post("/rest/2.0/xpan/file")
            .query(&[("method", "filemetas")])
            .form(&[("target", target.as_str()), ("dlink", "1")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let data = self.decode_json(response).await?;
        let raw = data
            .get("info")
            .or_else(|| data.get("list"))
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or(data);
        if let Some(errno) = raw.get("errno").and_then(|value| value.as_i64()) {
            if errno != 0 {
                return Err(format!("百度文件信息查询失败: {raw}"));
            }
        }
        if std::env::var_os("PANDOCK_WEBDAV_DEBUG").is_some() {
            let keys = raw
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            eprintln!("[pandock-debug] meta path={} keys={keys:?}", path);
        }
        let mut entry: FileEntry = serde_json::from_value(raw).map_err(|e| e.to_string())?;
        if entry.server_filename.is_empty() {
            entry.server_filename = path.rsplit('/').next().unwrap_or_default().to_string();
        }
        if entry.path.is_empty() {
            entry.path = path;
        }
        Ok(entry)
    }

    pub async fn mkdir(&self, dav_path: &str) -> Result<(), String> {
        self.require_child_path(dav_path)?;
        let path = self.remote_path(dav_path)?;
        let response = self
            .api_post("/rest/2.0/xpan/file")
            .query(&[("method", "create")])
            .form(&[
                ("path", path.as_str()),
                ("size", "0"),
                ("isdir", "1"),
                ("rtype", "0"),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        self.ensure_success(response).await
    }

    pub async fn file_manager(
        &self,
        opera: &str,
        filelist: serde_json::Value,
        ondup: &str,
    ) -> Result<(), String> {
        let response = self
            .api_post("/rest/2.0/xpan/file")
            .query(&[("method", "filemanager"), ("opera", opera)])
            // WebDAV operations must not return success while Baidu is still
            // processing an asynchronous task in the background.
            .form(&[
                ("async", "0"),
                ("ondup", ondup),
                ("filelist", filelist.to_string().as_str()),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let data = self.decode_json(response).await?;
        if data.get("taskid").is_some() {
            return Err("百度返回异步任务，操作尚未确认完成".into());
        }
        let info = data
            .get("info")
            .and_then(|value| value.as_array())
            .ok_or_else(|| "百度未返回文件操作结果".to_string())?;
        if info.len() != filelist.as_array().map_or(0, Vec::len) {
            return Err("百度文件操作结果数量不匹配".into());
        }
        for item in info {
            if item.get("errno").and_then(|value| value.as_i64()) != Some(0) {
                return Err(format!("百度文件操作失败: {item}"));
            }
        }
        Ok(())
    }

    pub async fn delete(&self, dav_path: &str) -> Result<(), String> {
        self.require_child_path(dav_path)?;
        self.file_manager(
            "delete",
            serde_json::json!([self.remote_path(dav_path)?]),
            "fail",
        )
        .await
    }

    pub async fn move_to(&self, from: &str, to: &str) -> Result<(), String> {
        self.move_to_with_overwrite(from, to, true).await
    }

    pub async fn move_to_with_overwrite(
        &self,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), String> {
        self.require_child_path(from)?;
        self.require_child_path(to)?;
        let (dest, name) = split_parent(to);
        self.file_manager(
            "move",
            serde_json::json!([{"path": self.remote_path(from)?, "dest": self.remote_path(dest)?, "newname": name}]),
            if overwrite { "overwrite" } else { "fail" },
        )
        .await
    }

    pub async fn copy_to(&self, from: &str, to: &str) -> Result<(), String> {
        self.copy_to_with_overwrite(from, to, true).await
    }

    pub async fn copy_to_with_overwrite(
        &self,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), String> {
        self.require_child_path(from)?;
        self.require_child_path(to)?;
        let (dest, name) = split_parent(to);
        self.file_manager(
            "copy",
            serde_json::json!([{"path": self.remote_path(from)?, "dest": self.remote_path(dest)?, "newname": name}]),
            if overwrite { "overwrite" } else { "fail" },
        )
        .await
    }

    pub async fn download_range(
        &self,
        dlink: &str,
        start: u64,
        count: usize,
    ) -> Result<bytes::Bytes, String> {
        if count == 0 {
            return Ok(bytes::Bytes::new());
        }
        let end = start.saturating_add(count as u64).saturating_sub(1);
        let url = format!(
            "{}{}access_token={}",
            dlink,
            if dlink.contains('?') { "&" } else { "?" },
            self.token
        );
        let response = self
            .http
            .get(url)
            .header("User-Agent", "pan.baidu.com")
            .header("Range", format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let content_length = response.content_length();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if std::env::var_os("PANDOCK_WEBDAV_DEBUG").is_some() {
            eprintln!(
                "[pandock-debug] download_range start={start} count={count} status={status} content_length={content_length:?} content_type={content_type:?}"
            );
        }
        if status != reqwest::StatusCode::PARTIAL_CONTENT && status != reqwest::StatusCode::OK {
            return Err(format!("百度未按请求范围返回文件数据: HTTP {status}"));
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
        {
            if std::env::var_os("PANDOCK_WEBDAV_DEBUG").is_some() {
                eprintln!("[pandock-debug] download_range returned JSON error");
            }
            return Err("百度下载链接返回了 JSON 错误而不是文件数据".into());
        }
        if let Some(range_start) = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes "))
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, _)| start.parse::<u64>().ok())
        {
            if range_start != start {
                return Err("百度返回的文件范围与请求不一致".into());
            }
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if std::env::var_os("PANDOCK_WEBDAV_DEBUG").is_some() {
            eprintln!(
                "[pandock-debug] download_range body_bytes={} requested_start={start} requested_count={count}",
                bytes.len()
            );
        }
        if status == reqwest::StatusCode::OK {
            if start != 0 || bytes.len() < count {
                return Err(format!(
                    "百度未按请求范围返回文件数据: 请求 {start}..{}, 返回 {} 字节",
                    start.saturating_add(count as u64),
                    bytes.len()
                ));
            }
            // Baidu sometimes ignores Range for small files and returns the
            // complete body. A range beginning at zero can safely be sliced.
            return Ok(bytes.slice(0..count));
        }
        if bytes.len() != count {
            return Err(format!(
                "百度返回字节数不匹配: 期望 {count}, 实际 {}",
                bytes.len()
            ));
        }
        Ok(bytes)
    }

    pub async fn download_to(&self, dav_path: &str, target: &Path) -> Result<FileEntry, String> {
        let meta = self.meta(dav_path).await?;
        let dlink = meta
            .dlink
            .clone()
            .ok_or_else(|| "百度未返回下载直链".to_string())?;
        let response = self
            .http
            .get(format!(
                "{}{}access_token={}",
                dlink,
                if dlink.contains('?') { "&" } else { "?" },
                self.token
            ))
            .header("User-Agent", "pan.baidu.com")
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let mut output = tokio::fs::File::create(target)
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = stream.next().await {
            output
                .write_all(&chunk.map_err(|e| e.to_string())?)
                .await
                .map_err(|e| e.to_string())?;
        }
        output.flush().await.map_err(|e| e.to_string())?;
        Ok(meta)
    }

    pub async fn upload_file(&self, dav_path: &str, local: &Path, size: u64) -> Result<(), String> {
        // WebDAV PUT 语义保持 rtype=3（覆盖同名）——ADR-112 定稿：此路径**不动**。
        self.upload_file_detailed(dav_path, local, size, 3)
            .await
            .map(|_| ())
    }

    /// 详细上传（ADR-112）：`rtype` 由调用方给定（NativeSync 用 0 = 冲突失败以便检测）；
    /// 回执含 create 响应的 fs_id/md5/mtime 与 `rapid`（precreate 返回待传分片为空 = 秒传候选，
    /// 秒传假冲突守卫据此决定先比 md5 再判冲突）。
    pub async fn upload_file_detailed(
        &self,
        dav_path: &str,
        local: &Path,
        size: u64,
        rtype: u32,
    ) -> Result<UploadOutcome, String> {
        let prepared = self
            .upload_precreate_and_parts(dav_path, local, size, rtype)
            .await?;
        // errno -8（rtype=0 冲突）在这里以 Err 返回，错误文本含 JSON 原文，
        // 由 CloudError::from_message 分类为 AlreadyExists（ADR-112 §3.3 冲突入口）。
        let created = self
            .upload_create(
                &prepared.path,
                size,
                rtype,
                &prepared.upload_id,
                &prepared.block_list,
            )
            .await?;
        Ok(UploadOutcome {
            fs_id: created.get("fs_id").and_then(|v| v.as_u64()),
            md5: created
                .get("md5")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            size,
            mtime: created.get("mtime").and_then(|v| v.as_i64()),
        })
    }

    /// 三段式的前两段：precreate + 分片上传 → 待 create 的全部材料。
    async fn upload_precreate_and_parts(
        &self,
        dav_path: &str,
        local: &Path,
        size: u64,
        rtype: u32,
    ) -> Result<UploadPrepared, String> {
        self.require_child_path(dav_path)?;
        let path = self.remote_path(dav_path)?;
        let mut file = tokio::fs::File::open(local)
            .await
            .map_err(|e| e.to_string())?;
        let mut hashes = Vec::new();
        let part_count = size.div_ceil(CHUNK_SIZE as u64).max(1);
        if part_count > 1024 {
            return Err("文件超过百度网盘 1024 个分片限制".into());
        }
        for part_index in 0..part_count {
            let remaining = size - part_index * CHUNK_SIZE as u64;
            let mut chunk = vec![0u8; remaining.min(CHUNK_SIZE as u64) as usize];
            if !chunk.is_empty() {
                file.read_exact(&mut chunk)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            hashes.push(format!("{:x}", md5::compute(&chunk)));
        }
        if file.metadata().await.map_err(|e| e.to_string())?.len() != size {
            return Err("上传文件大小在读取期间发生变化".into());
        }
        let block_list = serde_json::to_string(&hashes).map_err(|e| e.to_string())?;
        let response = self
            .api_post("/rest/2.0/xpan/file")
            .query(&[("method", "precreate")])
            .form(&[
                ("path", path.as_str()),
                ("size", &size.to_string()),
                ("isdir", "0"),
                ("autoinit", "1"),
                ("rtype", rtype.to_string().as_str()),
                ("block_list", block_list.as_str()),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let pre = self.decode_json(response).await?;
        let upload_id = pre
            .get("uploadid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "预上传未返回 uploadid".to_string())?
            .to_string();
        let mut needed = pre
            .get("block_list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if needed.is_empty() {
            // 【2026-10-13 实测 + 文档】预上传《响应示例》原文：「block_list 为空时等价于 [0]」——
            // 空列表**不是**秒传信号，而是照常传 0 号分片。实测（EXP1/2/3：全新内容/同内容/
            // 显式双 md5）precreate 恒返回 [0]，不传直接 create 实测 errno=31500（非文档 31363）。
            // 不要再次把它"优化"成跳过上传——那次修正（"真秒传"）已被实证回滚。
            needed.push(serde_json::json!(0));
        }
        let host = self.locate_upload(&path, &upload_id).await?;
        let mut uploaded_hashes = hashes.clone();
        for part in needed {
            let seq = part
                .as_u64()
                .ok_or_else(|| "百度返回非法分片序号".to_string())? as usize;
            let offset = seq as u64 * CHUNK_SIZE as u64;
            if offset >= size && size != 0 {
                return Err(format!("百度返回越界分片序号: {seq}"));
            }
            let mut source = tokio::fs::File::open(local)
                .await
                .map_err(|e| e.to_string())?;
            source
                .seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| e.to_string())?;
            let remaining = size.saturating_sub(offset) as usize;
            let mut chunk = vec![0u8; remaining.min(CHUNK_SIZE)];
            if !chunk.is_empty() {
                source
                    .read_exact(&mut chunk)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let part = multipart::Part::bytes(chunk).file_name("part");
            let form = multipart::Form::new().part("file", part);
            let response = self
                .http
                .post(format!("{host}/rest/2.0/pcs/superfile2"))
                .query(&[
                    ("method", "upload"),
                    ("access_token", self.token.as_str()),
                    ("type", "tmpfile"),
                    ("path", path.as_str()),
                    ("uploadid", upload_id.as_str()),
                    ("partseq", &seq.to_string()),
                ])
                .multipart(form)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let uploaded = self.decode_json(response).await?;
            let Some(md5) = uploaded.get("md5").and_then(|v| v.as_str()) else {
                return Err(format!("百度分片上传未返回 MD5: 分片 {seq}"));
            };
            if seq >= uploaded_hashes.len() {
                uploaded_hashes.resize(seq + 1, String::new());
            }
            uploaded_hashes[seq] = md5.to_string();
        }
        let uploaded_block_list =
            serde_json::to_string(&uploaded_hashes).map_err(|e| e.to_string())?;
        Ok(UploadPrepared {
            path,
            upload_id,
            block_list: uploaded_block_list,
        })
    }

    /// 三段式的末段：create（合并分片）。独立出来以便调用方拿到 create 的原始 JSON
    /// （fs_id/md5/mtime 是回声抑制的基线来源；-8 冲突也要从这里冒出）。
    async fn upload_create(
        &self,
        path: &str,
        size: u64,
        rtype: u32,
        upload_id: &str,
        block_list: &str,
    ) -> Result<serde_json::Value, String> {
        let response = self
            .api_post("/rest/2.0/xpan/file")
            .query(&[("method", "create")])
            .form(&[
                ("path", path),
                ("size", &size.to_string()),
                ("isdir", "0"),
                ("rtype", rtype.to_string().as_str()),
                ("uploadid", upload_id),
                ("block_list", block_list),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        self.decode_json(response).await
    }

    async fn locate_upload(&self, path: &str, upload_id: &str) -> Result<String, String> {
        let response = self
            .http
            .get(format!("{}/rest/2.0/pcs/file", self.upload_api))
            .query(&[
                ("method", "locateupload"),
                ("appid", "250528"),
                ("access_token", self.token.as_str()),
                ("path", path),
                ("uploadid", upload_id),
                ("upload_version", "2.0"),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let data = self.decode_json(response).await?;
        let server = data
            .get("servers")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .and_then(|v| v.get("server"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                data.get("bak_servers")
                    .and_then(|v| v.as_array())
                    .and_then(|v| v.first())
                    .and_then(|v| v.get("server"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.trim_end_matches('/').to_string())
            .ok_or_else(|| "获取上传域名失败".to_string())?;
        if !server.starts_with("https://") && !server.starts_with("http://") {
            return Err(format!("百度返回了无效上传域名: {server}"));
        }
        Ok(server)
    }

    fn api_get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{}", self.pan_api, path))
            .query(&[("access_token", self.token.as_str())])
    }

    fn api_post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{}", self.pan_api, path))
            .query(&[("access_token", self.token.as_str())])
    }

    async fn decode_json(&self, response: reqwest::Response) -> Result<serde_json::Value, String> {
        let status = response.status();
        let body = response.text().await.map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| format!("百度 API 返回非 JSON（HTTP {status}）"))?;
        let errno = value
            .get("errno")
            .and_then(|v| v.as_i64())
            .or_else(|| value.get("error_code").and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if !status.is_success() || errno != 0 || value.get("error").is_some() {
            return Err(format!("百度 API 错误：{body}"));
        }
        Ok(value)
    }

    async fn ensure_success(&self, response: reqwest::Response) -> Result<(), String> {
        self.decode_json(response).await.map(|_| ())
    }
}

fn api_error_code(error: &str) -> Option<i64> {
    let json = error.get(error.find('{')?..)?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("errno").and_then(|value| value.as_i64())
}

pub fn api_error_has_code(error: &str, code: i64) -> bool {
    let Some(start) = error.find('{') else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&error[start..]) else {
        return false;
    };
    json_value_has_error_code(&value, code)
}

fn json_value_has_error_code(value: &serde_json::Value, code: i64) -> bool {
    value.get("errno").and_then(|value| value.as_i64()) == Some(code)
        || value.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| json_value_has_error_code(item, code))
        })
        || value.as_object().is_some_and(|object| {
            object
                .values()
                .any(|item| json_value_has_error_code(item, code))
        })
}

fn split_parent(path: &str) -> (&str, &str) {
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(0) | None => ("/", path.trim_start_matches('/')),
        Some(idx) => (&path[..idx], &path[idx + 1..]),
    }
}

pub fn unix_time(seconds: Option<i64>) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(seconds.unwrap_or(0).max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> BaiduClient {
        BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
    }

    #[test]
    fn maps_dav_paths_into_application_directory() {
        assert_eq!(client().remote_path("/").unwrap(), "/apps/Demo");
        assert_eq!(
            client().remote_path("/folder/file.txt").unwrap(),
            "/apps/Demo/folder/file.txt"
        );
    }

    #[test]
    fn rejects_parent_traversal() {
        assert!(client().remote_path("/folder/../secret").is_err());
    }

    #[tokio::test]
    async fn root_writes_are_rejected_before_any_cloud_request() {
        let client = client();
        assert!(client.mkdir("/").await.is_err());
        assert!(client.delete("/").await.is_err());
        assert!(client.move_to("/", "/other").await.is_err());
        assert!(client.copy_to("/a", "/").await.is_err());
        assert!(client
            .upload_file("/", Path::new("not-read"), 1)
            .await
            .is_err());
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;
    use wiremock::{
        matchers::{body_string_contains, method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn device_flow_recognizes_pending_even_when_oauth_returns_http_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/oauth/2.0/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"error":"authorization_pending"})),
            )
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "".into())
            .with_base_urls(&server.uri());
        assert_eq!(
            client.poll_device_token("device-1").await.unwrap_err(),
            "PENDING"
        );
    }

    #[tokio::test]
    async fn device_flow_reports_denied_authorization_without_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/oauth/2.0/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error":"access_denied"})),
            )
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "".into())
            .with_base_urls(&server.uri());
        assert!(client
            .poll_device_token("device-1")
            .await
            .unwrap_err()
            .contains("access_denied"));
    }

    #[tokio::test]
    async fn file_manager_waits_for_synchronous_result_and_checks_item_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemanager"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"errno": 3101, "path": "/apps/Demo/a.txt"}]
            })))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        assert!(client.delete("/a.txt").await.is_err());
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(
            body.contains("async=0"),
            "filemanager must be synchronous: {body}"
        );
    }

    #[tokio::test]
    async fn move_and_copy_use_webdav_overwrite_semantics() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemanager"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"errno": 0, "path": "/apps/Demo/b.txt"}]
            })))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        client.move_to("/a.txt", "/b.txt").await.unwrap();
        client.copy_to("/a.txt", "/c.txt").await.unwrap();
        let requests = server.received_requests().await.unwrap();
        for request in requests {
            let body = String::from_utf8_lossy(&request.body);
            assert!(
                body.contains("ondup=overwrite"),
                "MOVE/COPY must replace an existing destination: {body}"
            );
        }
    }

    #[tokio::test]
    async fn device_flow_maps_invalid_client_to_actionable_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/oauth/2.0/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error":"invalid_client"})),
            )
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "bad-secret".into(), "Demo".into(), "".into())
            .with_base_urls(&server.uri());
        let error = client.poll_device_token("device-1").await.unwrap_err();
        assert!(error.contains("App Key 与 Secret Key 不匹配"), "{error}");
    }

    #[tokio::test]
    async fn range_download_accepts_full_body_from_zero() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/download"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into());
        let bytes = client
            .download_range(&format!("{}/download", server.uri()), 0, 3)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"hel");
    }

    #[tokio::test]
    async fn range_download_refuses_servers_that_ignore_the_range() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/download"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into());
        assert!(client
            .download_range(&format!("{}/download", server.uri()), 2, 3)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn range_download_refuses_mismatched_content_range() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/download"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 0-2/5")
                    .set_body_bytes(b"hel".to_vec()),
            )
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into());
        assert!(client
            .download_range(&format!("{}/download", server.uri()), 2, 3)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn list_and_meta_use_application_scope_and_dlink() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "list"))
            .and(query_param("dir", "/apps/Demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "list": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000}]
            })))
            .mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "info": [{"server_filename":"hello.txt","path":"/apps/Demo/hello.txt","fs_id":1,"size":5,"isdir":0,"server_mtime":1700000000,"dlink":"http://example.invalid/file"}]
            })))
            .mount(&server).await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        let list = client.list("/").await.unwrap();
        assert_eq!(list[0].server_filename, "hello.txt");
        let meta = client.meta("/hello.txt").await.unwrap();
        assert_eq!(meta.dlink.as_deref(), Some("http://example.invalid/file"));
    }

    #[tokio::test]
    async fn upload_detailed_uses_rtype0_and_returns_create_outcome() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("f.bin");
        std::fs::write(&local, b"hello").unwrap();
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "precreate"))
            .and(body_string_contains("rtype=0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "uploadid": "u1", "block_list": [0]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/pcs/file"))
            .and(query_param("method", "locateupload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error_code": 0, "servers": [{"server": server.uri()}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/pcs/superfile2"))
            .and(query_param("method", "upload"))
            .and(query_param("partseq", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "md5": "5d41402abc4b2a76b9719d911017c592"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "create"))
            .and(body_string_contains("rtype=0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "fs_id": 4242, "md5": "5d41402abc4b2a76b9719d911017c592",
                "size": 5, "mtime": 1700000123, "ctime": 1700000123, "path": "/apps/Demo/f.bin"
            })))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        let outcome = client
            .upload_file_detailed("/f.bin", &local, 5, 0)
            .await
            .unwrap();
        assert_eq!(outcome.fs_id, Some(4242));
        assert_eq!(
            outcome.md5.as_deref(),
            Some("5d41402abc4b2a76b9719d911017c592")
        );
        assert_eq!(outcome.mtime, Some(1700000123));
        assert_eq!(outcome.size, 5);
    }

    #[tokio::test]
    async fn upload_detailed_empty_block_list_uploads_part_zero_anyway() {
        // 【2026-10-13 实证回滚】文档「block_list 为空时等价于 [0]」：空列表必须照常传 0 号
        // 分片（否则 create 实测 errno=31500）。superfile2/locateupload 缺 mock 即 404 报错——
        // 成功即证明空列表没有被"优化"成跳过上传。
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("f.bin");
        std::fs::write(&local, b"hello").unwrap();
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "precreate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "uploadid": "u1", "block_list": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/pcs/file"))
            .and(query_param("method", "locateupload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error_code": 0, "servers": [{"server": server.uri()}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/pcs/superfile2"))
            .and(query_param("method", "upload"))
            .and(query_param("partseq", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "md5": "5d41402abc4b2a76b9719d911017c592"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "fs_id": 777, "md5": "5d41402abc4b2a76b9719d911017c592",
                "size": 5, "mtime": 1700000222
            })))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        let outcome = client
            .upload_file_detailed("/f.bin", &local, 5, 0)
            .await
            .unwrap();
        assert_eq!(outcome.fs_id, Some(777));
    }

    #[tokio::test]
    async fn upload_file_keeps_webdav_rtype3() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("f.bin");
        std::fs::write(&local, b"hello").unwrap();
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "precreate"))
            .and(body_string_contains("rtype=3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "uploadid": "u1", "block_list": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/pcs/file"))
            .and(query_param("method", "locateupload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error_code": 0, "servers": [{"server": server.uri()}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/pcs/superfile2"))
            .and(query_param("method", "upload"))
            .and(query_param("partseq", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "md5": "5d41402abc4b2a76b9719d911017c592"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "create"))
            .and(body_string_contains("rtype=3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "fs_id": 1, "size": 5
            })))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        // rtype 不匹配时 wiremock 不命中 ⇒ 404 ⇒ Err——所以成功即证明两段都是 rtype=3。
        client.upload_file("/f.bin", &local, 5).await.unwrap();
    }

    #[tokio::test]
    async fn upload_detailed_create_minus8_classifies_as_already_exists() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("f.bin");
        std::fs::write(&local, b"hello").unwrap();
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "precreate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0, "uploadid": "u1", "block_list": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/pcs/file"))
            .and(query_param("method", "locateupload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error_code": 0, "servers": [{"server": server.uri()}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/pcs/superfile2"))
            .and(query_param("method", "upload"))
            .and(query_param("partseq", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "md5": "5d41402abc4b2a76b9719d911017c592"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -8, "request_id": 12345
            })))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        let failure = client
            .upload_file_detailed("/f.bin", &local, 5, 0)
            .await
            .unwrap_err();
        // ADR-112 §3.3 冲突入口：-8 必须能分类为 AlreadyExists（含原始 errno）。
        let cloud = crate::cloud::CloudError::from_message(failure);
        assert_eq!(cloud.kind, crate::cloud::CloudErrorKind::AlreadyExists);
        assert_eq!(cloud.raw_errno, Some(-8));
        assert!(!cloud.retryable, "冲突不可重试（要走冲突流程，不是盲重）");
    }

    #[tokio::test]
    async fn refresh_token_errors_do_not_echo_the_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/oauth/2.0/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "expired"
            })))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "".into())
            .with_base_urls(&server.uri());
        let error = client
            .refresh_token("super-secret-refresh-token")
            .await
            .unwrap_err();
        assert!(error.contains("重新连接"), "{error}");
        assert!(!error.contains("super-secret-refresh-token"), "{error}");
        assert!(!error.contains("client_secret"), "{error}");
    }

    #[tokio::test]
    async fn meta_accepts_list_response_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "filemetas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "list": [{
                    "server_filename": "hello.txt",
                    "path": "/apps/Demo/hello.txt",
                    "fs_id": 1,
                    "size": 5,
                    "isdir": 0,
                    "server_mtime": 1700000000,
                    "dlink": "http://example.invalid/file"
                }]
            })))
            .mount(&server)
            .await;
        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        let meta = client.meta("/hello.txt").await.unwrap();
        assert_eq!(meta.size, 5);
        assert_eq!(meta.dlink.as_deref(), Some("http://example.invalid/file"));
    }

    #[tokio::test]
    async fn ensure_app_root_creates_missing_application_directory() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "list"))
            .and(query_param("dir", "/apps/Demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": -9,
                "request_id": 1
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "path": "/apps/Demo",
                "isdir": 1
            })))
            .mount(&server)
            .await;

        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        client.ensure_app_root().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let create = requests
            .iter()
            .find(|request| {
                request
                    .url
                    .query()
                    .unwrap_or_default()
                    .contains("method=create")
            })
            .expect("missing app directory must be created");
        let body = String::from_utf8_lossy(&create.body);
        assert!(body.contains("path="), "{body}");
        assert!(body.contains("isdir=1"), "{body}");
        assert!(body.contains("rtype=0"), "{body}");
    }

    #[tokio::test]
    async fn ensure_app_root_accepts_an_existing_root() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/2.0/xpan/file"))
            .and(query_param("method", "list"))
            .and(query_param("dir", "/apps/Demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errno": 0,
                "list": []
            })))
            .mount(&server)
            .await;

        let client = BaiduClient::new("key".into(), "secret".into(), "Demo".into(), "token".into())
            .with_base_urls(&server.uri());
        client.ensure_app_root().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
    }
}
