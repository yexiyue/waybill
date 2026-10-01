//! 权限、同步与进程锁属于原生存储；锁文件不删除，避免锁 inode 被替换。
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use waybill::{
    BoxFuture,
    checkpoint::{Checkpoint, CheckpointLease, CheckpointStore},
    error::{Error, ErrorKind, Result},
};
const LIMIT: usize = 1024 * 1024;
/// 持久 checkpoint 目录必须由宿主独占管理，不能放在不可信用户可写目录。
#[derive(Clone)]
pub struct FileCheckpointStore {
    root: PathBuf,
}
impl FileCheckpointStore {
    /// 只保存目录配置；实际 IO 在获取租约后进行。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}
struct Lease {
    path: PathBuf,
    lock: Arc<File>,
}
impl CheckpointStore for FileCheckpointStore {
    fn acquire<'a>(&'a self, operation: &'a str) -> BoxFuture<'a, Box<dyn CheckpointLease>> {
        Box::pin(async move {
            if operation.is_empty() || operation.len() > 128 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "invalid checkpoint key",
                ));
            }
            let root = self.root.clone();
            let key = blake3::hash(operation.as_bytes()).to_hex().to_string();
            blocking(move || {
                std::fs::create_dir_all(&root).map_err(io_error)?;
                std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                    .map_err(io_error)?;
                let lock = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .open(root.join(format!("{key}.lock")))
                    .map_err(io_error)?;
                fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        Error::new(ErrorKind::OperationBusy, "checkpoint operation locked")
                    } else {
                        io_error(error)
                    }
                })?;
                Ok(Box::new(Lease {
                    path: root.join(format!("{key}.json")),
                    lock: Arc::new(lock),
                }) as Box<dyn CheckpointLease>)
            })
            .await
        })
    }
}
impl CheckpointLease for Lease {
    fn load(&self) -> BoxFuture<'_, Option<Checkpoint>> {
        let path = self.path.clone();
        let lock = self.lock.clone();
        Box::pin(blocking(move || {
            let _guard = lock;
            let file = match File::open(path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(io_error(e)),
            };
            let mut data = Vec::new();
            file.take((LIMIT + 1) as u64)
                .read_to_end(&mut data)
                .map_err(io_error)?;
            if data.len() > LIMIT {
                return Err(Error::new(ErrorKind::Checkpoint, "checkpoint too large"));
            }
            decode_checkpoint(&data).map(Some)
        }))
    }
    fn save<'a>(&'a self, checkpoint: &'a Checkpoint) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut encoded = BoundedJson(Vec::new());
            serde_json::to_writer(&mut encoded, checkpoint).map_err(|e| {
                Error::new(ErrorKind::Checkpoint, "checkpoint encoding or size bound")
                    .with_source(e)
            })?;
            let data = encoded.0;
            let path = self.path.clone();
            let lock = self.lock.clone();
            blocking(move || {
                let _guard = lock;
                let dir = path
                    .parent()
                    .ok_or_else(|| Error::new(ErrorKind::Checkpoint, "checkpoint directory"))?;
                let mut file = tempfile::Builder::new()
                    .prefix(".pending-")
                    .tempfile_in(dir)
                    .map_err(io_error)?;
                file.as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(io_error)?;
                file.write_all(&data)
                    .and_then(|_| file.as_file().sync_all())
                    .map_err(io_error)?;
                file.persist(&path).map_err(|e| io_error(e.error))?;
                sync_directory(dir)
            })
            .await
        })
    }
    fn remove(&self) -> BoxFuture<'_, ()> {
        let path = self.path.clone();
        let lock = self.lock.clone();
        Box::pin(blocking(move || {
            let _guard = lock;
            match std::fs::remove_file(&path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(io_error(e)),
            }
            sync_directory(
                path.parent()
                    .ok_or_else(|| Error::new(ErrorKind::Checkpoint, "checkpoint directory"))?,
            )
        }))
    }
}
/// 解码有界的当前 checkpoint 信封；损坏记录保留并返回 Checkpoint。
/// 旧平铺格式没有兼容加载路径，未知信封版本由引擎明确拒绝。
pub fn decode_checkpoint(bytes: &[u8]) -> Result<Checkpoint> {
    if bytes.len() > LIMIT {
        return Err(Error::new(ErrorKind::Checkpoint, "checkpoint too large"));
    }
    serde_json::from_slice(bytes)
        .map_err(|e| Error::new(ErrorKind::Checkpoint, "invalid checkpoint JSON").with_source(e))
}
// 在序列化过程中限制分配，而不是先生成任意大的 JSON 再拒绝。
struct BoundedJson(Vec<u8>);
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > LIMIT.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("checkpoint size bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::new(ErrorKind::Checkpoint, "checkpoint worker").with_source(e))?
}
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(io_error)
}
fn io_error(e: std::io::Error) -> Error {
    Error::new(ErrorKind::Checkpoint, "checkpoint IO").with_source(e)
}
