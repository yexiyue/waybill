//! 本地下载目标：`.part` 随机写、同步、校验与同盘原子发布。
//!
//! 暂存与发布机制沿袭 SwarmDrop host-fs（`local_fs/part_file.rs` 与
//! `sink_ops.rs`，基线 `0a81f133`，MIT）并按本库契约改写，保留两条上游
//! 钉死的行为：pwrite 按字节精确偏移（不取整，上游 2026-07 事故教训）
//! 与发布失败必须保留 `.part` 供原地重试。`.part` 预分配长度，因此
//! 完成度只认区间账本，不认文件长度。
use crate::checkpoint::sync_directory;
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt;
use waybill::{
    BoxFuture,
    checkpoint::DriverState,
    download::{
        DigestAlgorithm, DownloadIntent, DownloadStatus, DownloadTarget, RemoteIdentity,
        Verification,
    },
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    upload::ConflictPolicy,
    upload::Receipt,
};

const HASH_BUFFER: usize = 256 * 1024;
const WRITE_LIMIT: usize = 32 * 1024 * 1024;
const TARGET_STATE_VERSION: u32 = 1;

/// 绑定一个 FsService 实例的本地目标；目标路径由下载意图携带。
pub struct LocalTarget {
    identity: ServiceIdentity,
}
impl LocalTarget {
    pub(crate) fn new(identity: ServiceIdentity) -> Self {
        Self { identity }
    }
}
/// 目标侧私有状态：冲突决策与初始化、校验事实。
#[derive(Serialize, Deserialize)]
struct TargetState {
    initialized: bool,
    verified: bool,
    /// 冲突策略决定后的实际目标路径；None 表示沿用意图原路径。
    effective: Option<String>,
}
/// `.part` 路径：目标文件名追加 `.part` 后缀。
fn part_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map_or_else(|| "download".into(), |n| n.to_os_string());
    name.push(".part");
    target.with_file_name(name)
}
/// 操作后缀名：`a.zip` → `a-<12hex>.zip`，与上传侧冲突命名同构；
/// 由操作 ID 决定，重跑落到同一目标路径。
fn conflict_name(target: &str, operation: &str) -> String {
    let key = blake3::hash(operation.as_bytes()).to_hex();
    let key = &key[..12];
    match target.rfind('.') {
        Some(index) if index > 0 && !target[..index].ends_with('/') => {
            format!("{}-{}.{}", &target[..index], key, &target[index + 1..])
        }
        _ => format!("{target}-{key}"),
    }
}
impl LocalTarget {
    fn decode(state: &DriverState) -> Result<TargetState> {
        if state.version != TARGET_STATE_VERSION {
            return Err(Error::new(
                ErrorKind::IncompatibleVersion,
                "local target state version",
            ));
        }
        serde_json::from_slice(&state.payload)
            .map_err(|_| Error::new(ErrorKind::Checkpoint, "invalid local target state"))
    }
    fn encode(state: &TargetState) -> DriverState {
        DriverState {
            version: TARGET_STATE_VERSION,
            payload: serde_json::to_vec(state).unwrap_or_default(),
        }
    }
    fn effective(&self, intent: &DownloadIntent, state: &TargetState) -> PathBuf {
        state.effective.as_deref().unwrap_or(&intent.target).into()
    }
    /// 已校验状态的证据级别；未校验不声明任何内容一致性。
    fn verification_of(source: &RemoteIdentity, verified: bool) -> Verification {
        if !verified {
            return Verification::Unverified;
        }
        match &source.digest {
            Some(digest) => Verification::Digest {
                algorithm: digest.algorithm,
                value: digest.value.clone(),
            },
            None => Verification::Length,
        }
    }
    fn receipt(
        &self,
        intent: &DownloadIntent,
        source: &RemoteIdentity,
        object: PathBuf,
        verified: Verification,
    ) -> Receipt {
        Receipt {
            operation: intent.operation.clone(),
            service: self.identity.clone(),
            target: intent.target.clone(),
            object: object.to_string_lossy().into_owned(),
            size: source.size,
            verified,
        }
    }
}
impl DownloadTarget for LocalTarget {
    fn identity(&self) -> ServiceIdentity {
        self.identity.clone()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_write: true,
            durable_publish: true,
            ..Capabilities::default()
        }
    }
    fn prepare<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        _source: &'a RemoteIdentity,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async move {
            intent.validate()?;
            let effective = match intent.conflict {
                ConflictPolicy::OperationSuffix
                    if tokio::fs::try_exists(&intent.target).await.unwrap_or(false) =>
                {
                    Some(conflict_name(&intent.target, &intent.operation))
                }
                _ => None,
            };
            Ok(LocalTarget::encode(&TargetState {
                initialized: false,
                verified: false,
                effective,
            }))
        })
    }
    fn probe<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus> {
        Box::pin(async move {
            let mut decoded = LocalTarget::decode(state)?;
            let dest = self.effective(intent, &decoded);
            let part = part_path(&dest);
            if !decoded.initialized {
                // 驱动未初始化：`.part` 即使存在也不可信，重建并截断。
                decoded.verified = false;
                return Ok(DownloadStatus::NeedsReset(LocalTarget::encode(&decoded)));
            }
            if tokio::fs::try_exists(&part).await.unwrap_or(false) {
                let length = tokio::fs::metadata(&part).await.map_err(io)?.len();
                if length == source.size {
                    return Ok(DownloadStatus::Ready {
                        state: LocalTarget::encode(&decoded),
                        verified: decoded.verified,
                    });
                }
                // 长度不符：暂存被外部改动，账本不可信，整体重建。
                decoded.initialized = false;
                decoded.verified = false;
                return Ok(DownloadStatus::NeedsReset(LocalTarget::encode(&decoded)));
            }
            if !tokio::fs::try_exists(&dest).await.unwrap_or(false) {
                decoded.initialized = false;
                decoded.verified = false;
                return Ok(DownloadStatus::NeedsReset(LocalTarget::encode(&decoded)));
            }
            // `.part` 缺失而目标存在：发布窗口对账。rename 是原子的，
            // 已校验状态配合长度即复原原证据；否则对目标重算摘要。
            let length = tokio::fs::metadata(&dest).await.map_err(io)?.len();
            if length != source.size {
                return Err(Error::new(ErrorKind::Conflict, "destination occupied"));
            }
            let verified = if decoded.verified {
                LocalTarget::verification_of(source, true)
            } else {
                hash_path(&dest, source).await?
            };
            decoded.verified = true;
            Ok(DownloadStatus::Complete {
                state: LocalTarget::encode(&decoded),
                receipt: self.receipt(intent, source, dest, verified),
            })
        })
    }
    fn initialize<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus> {
        Box::pin(async move {
            let mut decoded = LocalTarget::decode(state)?;
            let dest = self.effective(intent, &decoded);
            if intent.conflict == ConflictPolicy::Reject
                && decoded.effective.is_none()
                && tokio::fs::try_exists(&dest).await.unwrap_or(false)
            {
                return Err(Error::new(ErrorKind::Conflict, "destination occupied"));
            }
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(io)?;
            }
            // 预分配长度；账本清零后旧内容全部作废。
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(part_path(&dest))
                .await
                .map_err(io)?;
            if source.size > 0 {
                file.set_len(source.size).await.map_err(io)?;
            }
            drop(file);
            decoded.initialized = true;
            decoded.verified = false;
            Ok(DownloadStatus::Ready {
                state: LocalTarget::encode(&decoded),
                verified: false,
            })
        })
    }
    fn write_chunk<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
        offset: u64,
        data: Vec<u8>,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async move {
            let mut decoded = LocalTarget::decode(state)?;
            if !decoded.initialized {
                return Err(Error::new(ErrorKind::Checkpoint, "staging unavailable"));
            }
            let end = offset
                .checked_add(data.len() as u64)
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "write out of bounds"))?;
            if data.is_empty() || data.len() > WRITE_LIMIT || end > source.size {
                return Err(Error::new(ErrorKind::InvalidInput, "write out of bounds"));
            }
            let part = part_path(&self.effective(intent, &decoded));
            tokio::task::spawn_blocking(move || {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&part)
                    .map_err(io)?;
                // 精确偏移 pwrite，不取整、不依赖文件当前位置。
                file.write_all_at(&data, offset).map_err(io)?;
                // 数据写入并同步之后才允许记账。
                file.sync_all().map_err(io)?;
                Ok(())
            })
            .await
            .map_err(|e| Error::new(ErrorKind::Io, "staging worker").with_source(e))??;
            decoded.verified = false;
            Ok(LocalTarget::encode(&decoded))
        })
    }
    fn verify<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async move {
            let mut decoded = LocalTarget::decode(state)?;
            if !decoded.initialized {
                return Err(Error::new(ErrorKind::Checkpoint, "staging unavailable"));
            }
            let part = part_path(&self.effective(intent, &decoded));
            hash_path(&part, source).await?;
            decoded.verified = true;
            Ok(LocalTarget::encode(&decoded))
        })
    }
    fn publish<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus> {
        Box::pin(async move {
            let mut decoded = LocalTarget::decode(state)?;
            let dest = self.effective(intent, &decoded);
            let part = part_path(&dest);
            if !tokio::fs::try_exists(&part).await.unwrap_or(false) {
                return Err(Error::new(ErrorKind::Checkpoint, "staging unavailable"));
            }
            if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
                return Err(Error::new(ErrorKind::Conflict, "destination occupied"));
            }
            let verification = LocalTarget::verification_of(source, decoded.verified);
            let parent = dest.parent().map(Path::to_path_buf);
            // 同盘 rename 原子发布；失败保留 `.part`，重跑只做发布。
            tokio::fs::rename(&part, &dest).await.map_err(|e| {
                if e.raw_os_error() == Some(18) {
                    Error::new(ErrorKind::Unsupported, "cross-device publish unsupported")
                        .with_source(e)
                } else {
                    io(e)
                }
            })?;
            if let Some(parent) = parent {
                tokio::task::spawn_blocking(move || sync_directory(&parent))
                    .await
                    .map_err(|e| Error::new(ErrorKind::Io, "publish worker").with_source(e))??;
            }
            decoded.verified = true;
            Ok(DownloadStatus::Complete {
                state: LocalTarget::encode(&decoded),
                receipt: self.receipt(intent, source, dest, verification),
            })
        })
    }
}
/// 哈希本地路径并按可用证据声明一致性级别；长度永远是下限校验。
async fn hash_path(path: &Path, source: &RemoteIdentity) -> Result<Verification> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| Error::new(ErrorKind::Checkpoint, "staging unavailable"))?;
    if file.metadata().await.map_err(io)?.len() != source.size {
        return Err(Error::new(ErrorKind::Checkpoint, "staging length mismatch"));
    }
    let Some(digest) = &source.digest else {
        return Ok(Verification::Length);
    };
    let mut buffer = vec![0u8; HASH_BUFFER];
    let computed = match digest.algorithm {
        DigestAlgorithm::Blake3 => {
            let mut hasher = blake3::Hasher::new();
            loop {
                let read = file.read(&mut buffer).await.map_err(io)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            hasher.finalize().to_hex().to_string()
        }
        DigestAlgorithm::Md5 => {
            use md5::Digest as _;
            let mut hasher = md5::Md5::new();
            loop {
                let read = file.read(&mut buffer).await.map_err(io)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            format!("{:x}", hasher.finalize())
        }
    };
    if computed != digest.value {
        return Err(Error::new(ErrorKind::Checkpoint, "staging digest mismatch"));
    }
    Ok(Verification::Digest {
        algorithm: digest.algorithm,
        value: digest.value.clone(),
    })
}
fn io(error: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, "local target IO").with_source(error)
}
