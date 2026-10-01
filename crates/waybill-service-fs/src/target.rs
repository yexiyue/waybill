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
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt;
use waybill::{
    BoxFuture,
    checkpoint::DriverState,
    content::{DigestAlgorithm, Verification},
    download::{DownloadIntent, DownloadStatus, DownloadTarget, RemoteIdentity},
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    transfer::ConflictPolicy,
    transfer::Receipt,
};

const HASH_BUFFER: usize = 256 * 1024;
const WRITE_LIMIT: usize = 32 * 1024 * 1024;
const TARGET_STATE_VERSION: u32 = 2;

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
    /// 每次初始化创建独占随机暂存文件；不同操作不会共享写入位置。
    staging: Option<String>,
    file_identity: Option<FileIdentity>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}
impl FileIdentity {
    fn from(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}
/// 后缀只处理文件名，避免父目录中的点被误当成扩展名。
fn conflict_name(target: &str, operation: &str) -> String {
    let path = Path::new(target);
    let key = blake3::hash(operation.as_bytes()).to_hex();
    let mut name = path.file_stem().unwrap_or_default().to_os_string();
    name.push(format!("-{}", &key[..12]));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    path.with_file_name(name).to_string_lossy().into_owned()
}

/// 拒绝符号链接或替换后的暂存；记录绑定文件身份而非仅路径与长度。
async fn staging_metadata(state: &TargetState) -> Result<Option<std::fs::Metadata>> {
    let Some(path) = &state.staging else {
        return Ok(None);
    };
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(error)),
    };
    if !metadata.is_file() || state.file_identity.as_ref() != Some(&FileIdentity::from(&metadata)) {
        return Err(Error::new(
            ErrorKind::Checkpoint,
            "staging identity changed",
        ));
    }
    Ok(Some(metadata))
}
fn staging_path(state: &TargetState) -> Result<PathBuf> {
    state
        .staging
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| Error::new(ErrorKind::Checkpoint, "staging unavailable"))
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
    fn encode(state: &TargetState) -> Result<DriverState> {
        let payload = serde_json::to_vec(state).map_err(|error| {
            Error::new(ErrorKind::Checkpoint, "encode local target state").with_source(error)
        })?;
        Ok(DriverState {
            version: TARGET_STATE_VERSION,
            payload,
        })
    }
    fn effective(&self, intent: &DownloadIntent, state: &TargetState) -> Result<PathBuf> {
        let target = state.effective.as_deref().unwrap_or(&intent.target);
        if !valid_local_target(&intent.target) || !valid_local_target(target) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "local target requires an absolute file path",
            ));
        }
        Ok(target.into())
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
    async fn reconcile_published(
        &self,
        intent: &DownloadIntent,
        source: &RemoteIdentity,
        mut state: TargetState,
        dest: PathBuf,
        remove_staging_link: bool,
    ) -> Result<DownloadStatus> {
        let verified = hash_path(&dest, source).await?;
        if remove_staging_link {
            tokio::fs::remove_file(staging_path(&state)?)
                .await
                .map_err(io)?;
        }
        // 发布可能已完成但目录同步失败；对账仍须重试持久性步骤。
        sync_parent(&dest).await?;
        state.verified = true;
        Ok(DownloadStatus::Complete {
            state: Self::encode(&state)?,
            receipt: self.receipt(intent, source, dest, verified),
        })
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
/// 本地目标路径：非空、无控制字符、总长 ≤4096 字节；段非空且不为
/// `.`、`..`，必须是以 `/` 起始的绝对路径；反斜杠在 Linux / macOS 是
/// 合法文件名字节，不按远端路径规则禁止。
fn valid_local_target(target: &str) -> bool {
    if !target.starts_with('/')
        || target.is_empty()
        || target.len() > 4096
        || target.ends_with('/')
        || target.chars().any(|c| c.is_control())
    {
        return false;
    }
    let mut segments = target.split('/');
    if target.starts_with('/') {
        segments.next();
    }
    segments.all(|segment| {
        !segment.is_empty() && segment != "." && segment != ".." && segment.len() <= 255
    })
}
impl DownloadTarget for LocalTarget {
    fn max_write_size(&self) -> usize {
        32 * 1024 * 1024
    }

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
            if !valid_local_target(&intent.target) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "local target requires an absolute file path",
                ));
            }
            let effective = match intent.conflict {
                ConflictPolicy::OperationSuffix
                    if tokio::fs::try_exists(&intent.target).await.map_err(io)? =>
                {
                    Some(conflict_name(&intent.target, &intent.operation))
                }
                _ => None,
            };
            LocalTarget::encode(&TargetState {
                initialized: false,
                verified: false,
                effective,
                staging: None,
                file_identity: None,
            })
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
            let dest = self.effective(intent, &decoded)?;
            if !decoded.initialized {
                // 驱动未初始化：不信任已有暂存，创建新的独占文件。
                decoded.verified = false;
                return Ok(DownloadStatus::NeedsReset(LocalTarget::encode(&decoded)?));
            }
            if let Some(metadata) = staging_metadata(&decoded).await? {
                let length = metadata.len();
                if length == source.size {
                    // 无覆盖发布的硬链接回退可能保留原名；同 inode 是发布证据。
                    if decoded.verified {
                        match tokio::fs::symlink_metadata(&dest).await {
                            Ok(dest_meta)
                                if dest_meta.is_file()
                                    && FileIdentity::from(&dest_meta)
                                        == FileIdentity::from(&metadata) =>
                            {
                                return self
                                    .reconcile_published(intent, source, decoded, dest, true)
                                    .await;
                            }
                            Ok(_) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(io(error)),
                        }
                    }
                    if decoded.verified {
                        hash_path(&staging_path(&decoded)?, source).await?;
                    }
                    return Ok(DownloadStatus::Ready {
                        state: LocalTarget::encode(&decoded)?,
                        verified: decoded.verified,
                    });
                }
                // 长度不符：暂存被外部改动，账本不可信，整体重建。
                decoded.initialized = false;
                decoded.verified = false;
                return Ok(DownloadStatus::NeedsReset(LocalTarget::encode(&decoded)?));
            }
            if !tokio::fs::try_exists(&dest).await.map_err(io)? {
                decoded.initialized = false;
                decoded.verified = false;
                return Ok(DownloadStatus::NeedsReset(LocalTarget::encode(&decoded)?));
            }
            // 暂存缺失而目标存在：发布窗口对账必须重新核验内容，
            // 旧 verified 标志不能为外部替换后的同长度文件提供证据。
            let metadata = tokio::fs::symlink_metadata(&dest).await.map_err(io)?;
            if !metadata.is_file() {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "destination is not a regular file",
                ));
            }
            let length = metadata.len();
            if length != source.size {
                return Err(Error::new(ErrorKind::Conflict, "destination occupied"));
            }
            self.reconcile_published(intent, source, decoded, dest, false)
                .await
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
            let dest = self.effective(intent, &decoded)?;
            if intent.conflict == ConflictPolicy::Reject
                && decoded.effective.is_none()
                && tokio::fs::try_exists(&dest).await.map_err(io)?
            {
                return Err(Error::new(ErrorKind::Conflict, "destination occupied"));
            }
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(io)?;
            }
            // 独占随机暂存文件：不打开或截断调用者已有的 <目标>.part。
            // 初始化结果落盘前崩溃最多留下孤立暂存，不会污染另一操作。
            let parent = dest
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let temporary = tempfile::Builder::new()
                .prefix(".wb-")
                .suffix(".part")
                .tempfile_in(parent)
                .map_err(io)?;
            temporary.as_file().set_len(source.size).map_err(io)?;
            temporary.as_file().sync_all().map_err(io)?;
            let file_identity = FileIdentity::from(&temporary.as_file().metadata().map_err(io)?);
            let (_, path) = temporary.keep().map_err(|error| io(error.error))?;
            sync_directory(parent)?;
            decoded.staging = Some(path.to_string_lossy().into_owned());
            decoded.file_identity = Some(file_identity);
            decoded.initialized = true;
            decoded.verified = false;
            Ok(DownloadStatus::Ready {
                state: LocalTarget::encode(&decoded)?,
                verified: false,
            })
        })
    }
    fn write_chunk<'a>(
        &'a self,
        _intent: &'a DownloadIntent,
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
            staging_metadata(&decoded)
                .await?
                .ok_or_else(|| Error::new(ErrorKind::Checkpoint, "staging unavailable"))?;
            let part = staging_path(&decoded)?;
            let expected_identity = decoded
                .file_identity
                .as_ref()
                .ok_or_else(|| Error::new(ErrorKind::Checkpoint, "staging identity missing"))?;
            // 先打开并核验句柄，再交给不可取消的阻塞写任务。
            // 若恢复随后重建暂存，旧任务仍只写旧 inode。
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&part)
                .map_err(io)?;
            if FileIdentity::from(&file.metadata().map_err(io)?) != *expected_identity {
                return Err(Error::new(
                    ErrorKind::Checkpoint,
                    "staging identity changed",
                ));
            }
            tokio::task::spawn_blocking(move || {
                // 精确偏移 pwrite，不取整、不依赖文件当前位置。
                file.write_all_at(&data, offset).map_err(io)?;
                // 数据写入并同步之后才允许记账。
                file.sync_all().map_err(io)?;
                Ok(())
            })
            .await
            .map_err(|e| Error::new(ErrorKind::Io, "staging worker").with_source(e))??;
            decoded.verified = false;
            LocalTarget::encode(&decoded)
        })
    }
    fn verify<'a>(
        &'a self,
        _intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async move {
            let mut decoded = LocalTarget::decode(state)?;
            if !decoded.initialized {
                return Err(Error::new(ErrorKind::Checkpoint, "staging unavailable"));
            }
            staging_metadata(&decoded)
                .await?
                .ok_or_else(|| Error::new(ErrorKind::Checkpoint, "staging unavailable"))?;
            let part = staging_path(&decoded)?;
            hash_path(&part, source).await?;
            decoded.verified = true;
            LocalTarget::encode(&decoded)
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
            if !decoded.verified {
                return Err(Error::new(
                    ErrorKind::Checkpoint,
                    "staging must be verified before publication",
                ));
            }
            let dest = self.effective(intent, &decoded)?;
            staging_metadata(&decoded)
                .await?
                .ok_or_else(|| Error::new(ErrorKind::Checkpoint, "staging unavailable"))?;
            let part = staging_path(&decoded)?;
            let verification = LocalTarget::verification_of(source, decoded.verified);
            // tempfile 使用无覆盖 rename，必要时回退到硬链接原子占位；
            // 即使另一操作同时创建目标，也不会被覆盖。失败时保留暂存路径。
            let staging = tempfile::TempPath::try_from_path(&part).map_err(io)?;
            if let Err(error) = staging.persist_noclobber(&dest) {
                let error_kind = error.error.kind();
                let raw = error.error.raw_os_error();
                let cause = error.error;
                let _ = error.path.keep();
                return Err(if error_kind == std::io::ErrorKind::AlreadyExists {
                    Error::new(ErrorKind::Conflict, "destination occupied").with_source(cause)
                } else if raw == Some(18) {
                    Error::new(ErrorKind::Unsupported, "cross-device publish unsupported")
                        .with_source(cause)
                } else {
                    io(cause)
                });
            }
            sync_parent(&dest).await?;
            decoded.verified = true;
            Ok(DownloadStatus::Complete {
                state: LocalTarget::encode(&decoded)?,
                receipt: self.receipt(intent, source, dest, verification),
            })
        })
    }
}
async fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().map(Path::to_path_buf) {
        tokio::task::spawn_blocking(move || sync_directory(&parent))
            .await
            .map_err(|error| Error::new(ErrorKind::Io, "publish worker").with_source(error))??;
    }
    Ok(())
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
        // 无可信源摘要时只能声明长度一致，不能检测同长度替换或损坏。
        return Ok(Verification::Length);
    };
    let computed = match digest.algorithm {
        DigestAlgorithm::Blake3 => {
            let mut hasher = blake3::Hasher::new();
            read_hash_chunks(&mut file, |bytes| {
                hasher.update(bytes);
            })
            .await?;
            hasher.finalize().to_hex().to_string()
        }
        DigestAlgorithm::Md5 => {
            use md5::Digest as _;
            let mut hasher = md5::Md5::new();
            read_hash_chunks(&mut file, |bytes| {
                hasher.update(bytes);
            })
            .await?;
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
/// 算法实现复用同一有界读取循环；哈希计算仍由各算法库负责。
async fn read_hash_chunks(file: &mut tokio::fs::File, mut update: impl FnMut(&[u8])) -> Result<()> {
    let mut buffer = vec![0u8; HASH_BUFFER];
    loop {
        let read = file.read(&mut buffer).await.map_err(io)?;
        if read == 0 {
            return Ok(());
        }
        update(&buffer[..read]);
    }
}
fn io(error: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, "local target IO").with_source(error)
}
