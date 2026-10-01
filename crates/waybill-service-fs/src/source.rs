//! 文件句柄与路径同时核验，避免重开时跟随已替换的源。
use std::{os::unix::fs::MetadataExt, path::PathBuf, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::Mutex,
};
use waybill::{
    BoxFuture,
    error::{Error, ErrorKind, Result},
    service::{Capabilities, Service, ServiceId, ServiceIdentity, ServiceInfo},
    source::{Source, SourceIdentity},
};

/// 已打开且经哈希确认的稳定文件。Debug 不显示路径。
pub struct FileSource {
    path: PathBuf,
    file: Mutex<tokio::fs::File>,
    identity: SourceIdentity,
}
impl FileSource {
    /// 打开普通文件并流式计算身份；宿主在上传期间禁止其他写入者。
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let path = tokio::fs::canonicalize(path.into())
            .await
            .map_err(io_error)?;
        let mut file = tokio::fs::File::open(&path).await.map_err(io_error)?;
        // Tokio 的内部 IO 复制缓冲也设上限，避免随请求块扩大。
        file.set_max_buf_size(256 * 1024);
        let metadata = file.metadata().await.map_err(io_error)?;
        if !metadata.is_file() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "source must be a regular file",
            ));
        }
        let before = fingerprint(&metadata);
        let digest = hash(&mut file).await?;
        if before != fingerprint(&file.metadata().await.map_err(io_error)?)
            || before != fingerprint(&tokio::fs::metadata(&path).await.map_err(io_error)?)
        {
            return Err(changed());
        }
        let identity = SourceIdentity {
            reference: blake3::hash(path.as_os_str().as_bytes())
                .to_hex()
                .to_string(),
            revision: before,
            size: metadata.len(),
            blake3: digest,
        };
        Ok(Self {
            path,
            file: Mutex::new(file),
            identity,
        })
    }
    async fn verify_metadata(&self, file: &tokio::fs::File) -> Result<()> {
        if fingerprint(&file.metadata().await.map_err(io_error)?) != self.identity.revision
            || fingerprint(
                &tokio::fs::metadata(&self.path)
                    .await
                    .map_err(|_| changed())?,
            ) != self.identity.revision
        {
            return Err(changed());
        }
        Ok(())
    }
}
impl Source for FileSource {
    fn identity(&self) -> BoxFuture<'_, SourceIdentity> {
        Box::pin(async move {
            let mut file = self.file.lock().await;
            self.verify_metadata(&file).await?;
            if hash(&mut file).await? != self.identity.blake3 {
                return Err(changed());
            }
            self.verify_metadata(&file).await?;
            Ok(self.identity.clone())
        })
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async move {
            if length > 8 * 1024 * 1024
                || offset
                    .checked_add(length as u64)
                    .is_none_or(|end| end > self.identity.size)
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "source range out of bounds",
                ));
            }
            let mut file = self.file.lock().await;
            self.verify_metadata(&file).await?;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(io_error)?;
            let mut data = vec![0; length];
            file.read_exact(&mut data).await.map_err(|_| changed())?;
            self.verify_metadata(&file).await?;
            Ok(data)
        })
    }
}
/// 最小只读 service：只实现 Source，不实现上传。
pub struct FsService {
    identity: ServiceIdentity,
}
impl FsService {
    /// instance 是消费者的稳定本地命名空间。
    pub fn new(instance: impl Into<String>) -> Result<Self> {
        let instance = instance.into();
        if instance.is_empty() || instance.len() > 256 {
            return Err(Error::new(ErrorKind::InvalidInput, "invalid fs instance"));
        }
        Ok(Self {
            identity: ServiceIdentity {
                service: ServiceId::parse("waybill:fs")?,
                instance,
            },
        })
    }
}
impl Service for FsService {
    fn info(&self) -> ServiceInfo {
        ServiceInfo {
            identity: self.identity.clone(),
            capabilities: Capabilities {
                range_source: true,
                ..Capabilities::default()
            },
        }
    }
    fn source<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Arc<dyn Source>> {
        Box::pin(async move { Ok(Arc::new(FileSource::open(reference).await?) as Arc<dyn Source>) })
    }
}
async fn hash(file: &mut tokio::fs::File) -> Result<String> {
    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(io_error)?;
    let mut digest = blake3::Hasher::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let n = file.read(&mut buffer).await.map_err(io_error)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(digest.finalize().to_hex().to_string())
}
fn fingerprint(m: &std::fs::Metadata) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec()
    )
}
fn changed() -> Error {
    Error::new(ErrorKind::SourceChanged, "stable source changed")
}
fn io_error(error: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, "file source IO").with_source(error)
}
