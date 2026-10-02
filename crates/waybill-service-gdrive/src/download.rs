//! 下载侧：范围读取源、路径解析与目录列表。
//!
//! 元数据每次复核（version / md5 / size 复合为不透明 revision）；
//! Google 原生文档没有二进制内容与 md5，明确拒绝而不是退化为导出。
use crate::{
    Gdrive,
    client::protocol,
    object::{DriveFile, escape_query, validate_id},
};
use std::sync::Arc;
use waybill::{
    BoxFuture,
    content::{Digest, DigestAlgorithm},
    download::{DownloadSource, RemoteIdentity},
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
};
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const MEDIA_CHUNK_LIMIT: usize = 8 * 1024 * 1024;

/// 云端文件的可读元数据；供宿主列表与路径解析展示。
#[derive(Debug, Clone)]
pub struct RemoteFile {
    /// Drive 文件 ID。
    pub id: String,
    /// 文件名。
    pub name: String,
    /// 字节长度；Google 原生文档为空。
    pub size: Option<u64>,
    /// MIME 类型。
    pub mime_type: String,
    /// 最后修改时间（RFC 3339，服务端返回原样）。
    pub modified_time: Option<String>,
    /// 是否为文件夹。
    pub folder: bool,
}
impl RemoteFile {
    fn from(file: &DriveFile) -> Self {
        Self {
            id: file.id.clone(),
            name: file.name.clone(),
            size: file.size.as_deref().and_then(|s| s.parse().ok()),
            mime_type: file.mime_type.clone(),
            modified_time: file.modified_time.clone(),
            folder: file.mime_type == FOLDER_MIME,
        }
    }
}
impl From<RemoteFile> for waybill::object::ObjectMetadata {
    fn from(file: RemoteFile) -> Self {
        Self {
            reference: file.id,
            name: file.name,
            size: file.size,
            modified: file.modified_time,
            kind: if file.folder {
                waybill::object::ObjectKind::Directory
            } else {
                waybill::object::ObjectKind::File
            },
        }
    }
}
/// 路径解析结果：文件交给下载源，文件夹供列表。
#[derive(Debug, Clone)]
pub enum Resolved {
    /// 命中文件。
    File(RemoteFile),
    /// 命中文件夹（含根目录本身）。
    Folder {
        /// 文件夹 ID。
        id: String,
    },
}
/// 绑定单个 Drive 文件的下载源。
pub struct GdriveMedia {
    service: ServiceIdentity,
    api: Arc<crate::client::DriveClient>,
    reference: String,
}
impl DownloadSource for GdriveMedia {
    fn max_read_size(&self) -> usize {
        8 * 1024 * 1024
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_download: true,
            ..Default::default()
        }
    }
    fn identity(&self) -> BoxFuture<'_, RemoteIdentity> {
        let reference = self.reference.as_str();
        Box::pin(async move {
            let file = metadata(self.api.clone(), reference).await?;
            let size = file
                .size
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "Drive native document cannot be downloaded",
                    )
                })?;
            if file.mime_type.starts_with("application/vnd.google-apps.") {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Drive native document cannot be downloaded",
                ));
            }
            let md5 = match file.md5_checksum.as_deref() {
                Some(value) if valid_md5(value) => Some(Digest {
                    algorithm: DigestAlgorithm::Md5,
                    value: value.to_string(),
                }),
                Some(_) => return Err(protocol()),
                None => None,
            };
            let revision = format!(
                "{}:{}:{}",
                file.version.as_deref().unwrap_or_default(),
                file.md5_checksum.as_deref().unwrap_or_default(),
                size
            );
            Ok(RemoteIdentity {
                service: self.service.clone(),
                reference: file.id,
                revision,
                size,
                digest: md5,
            })
        })
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        let reference = self.reference.as_str();
        let api = Arc::clone(&self.api);
        Box::pin(async move {
            if length == 0 || length > MEDIA_CHUNK_LIMIT {
                return Err(Error::new(ErrorKind::InvalidInput, "media range bound"));
            }
            api.read_media_range(reference, offset, length).await
        })
    }
}
fn valid_md5(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
async fn metadata(api: Arc<crate::client::DriveClient>, reference: &str) -> Result<DriveFile> {
    api.get_file(reference)
        .await?
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "Drive file unavailable"))
}
impl Gdrive {
    /// 解析配置根下的相对路径；结尾 `/` 视为文件夹期望。
    /// 同名多义时拒绝（Drive 允许同目录同名文件）。
    pub async fn resolve(&self, path: &str) -> Result<Resolved> {
        let relative = path.strip_suffix('/').unwrap_or(path);
        if path != "/" && !waybill::object::valid_object_path(relative) {
            return Err(Error::new(ErrorKind::InvalidInput, "invalid Drive path"));
        }
        let expects_folder = path.ends_with('/');
        let segments: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        if segments.len() > 32 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Drive directory depth bound",
            ));
        }
        let mut parent = self.root_folder().await?;
        if segments.is_empty() {
            return Ok(Resolved::Folder { id: parent });
        }
        for (index, segment) in segments.iter().enumerate() {
            if segment.len() > 1024 || segment.chars().any(|c| c.is_control()) {
                return Err(Error::new(ErrorKind::InvalidInput, "invalid path segment"));
            }
            let last = index + 1 == segments.len();
            // 中间段必须是文件夹；结尾段按期望决定是否限定文件夹。
            let folder_only = !last || expects_folder;
            let mut query = format!(
                "trashed = false and '{}' in parents and name = '{}'",
                escape_query(&parent),
                escape_query(segment)
            );
            if folder_only {
                query.push_str(" and mimeType = 'application/vnd.google-apps.folder'");
            }
            let found = self.api.find_files(&query).await?;
            let unique = match found.as_slice() {
                [one] => one,
                [] => {
                    return Err(Error::new(ErrorKind::NotFound, "Drive path not found"));
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "ambiguous Drive path segment",
                    ));
                }
            };
            if !last {
                parent = unique.id.clone();
            } else if unique.mime_type == FOLDER_MIME {
                return Ok(Resolved::Folder {
                    id: unique.id.clone(),
                });
            } else if expects_folder {
                return Err(Error::new(ErrorKind::NotFound, "Drive path not found"));
            } else {
                return Ok(Resolved::File(RemoteFile::from(unique)));
            }
        }
        Err(protocol())
    }
    /// 列出文件夹直接子项；上限沿用协议层的 1000 项列表约束。
    pub async fn list(&self, folder: &str) -> Result<Vec<RemoteFile>> {
        validate_id(folder)?;
        let query = format!("trashed = false and '{}' in parents", escape_query(folder));
        Ok(self
            .api
            .find_files(&query)
            .await?
            .iter()
            .map(RemoteFile::from)
            .collect())
    }
}
/// 供宿主以文件 ID 直接构造下载源（绕过路径解析）。
impl Gdrive {
    pub(crate) fn media(&self, reference: &str) -> Result<GdriveMedia> {
        validate_id(reference)?;
        Ok(GdriveMedia {
            service: self.identity.clone(),
            api: Arc::clone(&self.api),
            reference: reference.to_string(),
        })
    }
}
