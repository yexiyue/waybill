//! 每次范围读取绑定不透明强 ETag；不将 ETag 当作内容哈希。
use crate::{ObjectStorage, error};
use opendal::Metadata;
use std::sync::Arc;
use waybill::{
    BoxFuture,
    download::{DownloadSource, RemoteIdentity},
    error::{Error, ErrorKind, Result},
    service::Capabilities,
};
const READ_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) struct Media {
    service: ObjectStorage,
    reference: String,
    key: String,
    etag: String,
    size: u64,
}
impl ObjectStorage {
    pub(crate) async fn open_media(&self, reference: &str) -> Result<Arc<dyn DownloadSource>> {
        if !self.download_supported() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "conditional range read required",
            ));
        }
        if reference.ends_with('/') {
            return Err(crate::invalid());
        }
        let key = self.key(reference)?;
        self.media(reference, key).await
    }
    pub(crate) async fn media(
        &self,
        reference: &str,
        key: String,
    ) -> Result<Arc<dyn DownloadSource>> {
        let metadata = self.bounded_operator.stat(&key).await.map_err(error::map)?;
        let etag = revision(&metadata)?;
        let media = Media {
            service: self.clone(),
            reference: reference.into(),
            key,
            etag,
            size: metadata.content_length(),
        };
        if media.size > 0 {
            media.read_range(0, 1).await?;
        }
        Ok(Arc::new(media))
    }
}
impl DownloadSource for Media {
    fn read_buffer_multiplier(&self) -> usize {
        2
    }
    fn max_read_size(&self) -> usize {
        READ_LIMIT
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_download: true,
            ..Default::default()
        }
    }
    fn identity(&self) -> BoxFuture<'_, RemoteIdentity> {
        Box::pin(async {
            let meta = self
                .service
                .bounded_operator
                .stat(&self.key)
                .await
                .map_err(error::map)?;
            if revision(&meta)? != self.etag || meta.content_length() != self.size {
                return Err(changed());
            }
            Ok(RemoteIdentity {
                service: self.service.identity.clone(),
                reference: self.reference.clone(),
                revision: self.etag.clone(),
                size: self.size,
                digest: None,
            })
        })
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async move {
            if length == 0
                || length > READ_LIMIT
                || offset
                    .checked_add(length as u64)
                    .is_none_or(|end| end > self.size)
            {
                return Err(crate::invalid());
            }
            let data = self
                .service
                .bounded_operator
                .read_with(&self.key)
                .range(offset..offset + length as u64)
                .if_match(&self.etag)
                .await
                .map_err(error::map)?;
            if data.len() != length {
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "object range length mismatch",
                ));
            }
            Ok(data.to_vec())
        })
    }
}
pub(crate) fn revision(metadata: &Metadata) -> Result<String> {
    if metadata.is_dir() {
        return Err(crate::invalid());
    }
    metadata
        .etag()
        .filter(|tag| {
            !tag.is_empty()
                && tag.len() <= 256
                && !tag.starts_with("W/")
                && !tag.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .ok_or_else(|| Error::new(ErrorKind::Unsupported, "strong object revision required"))
}
pub(crate) fn changed() -> Error {
    Error::new(
        ErrorKind::SourceChanged,
        "object version or content changed",
    )
}
