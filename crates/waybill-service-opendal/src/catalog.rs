//! 对象前缀映射为目录；流式分页，限制子项数量和可展示元数据总量。
use crate::{ObjectStorage, directory, error, validate_path};
use futures_util::StreamExt;
use waybill::{
    error::{Error, ErrorKind, Result},
    object::{ObjectKind, ObjectMetadata},
};
const ENTRY_LIMIT: usize = 1000;
const METADATA_LIMIT: usize = 1024 * 1024;
impl ObjectStorage {
    pub(crate) async fn resolve_object(&self, reference: &str) -> Result<ObjectMetadata> {
        let key = self.key(reference)?;
        if reference == "/" {
            return Ok(metadata(reference, true, None));
        }
        if reference.ends_with('/') {
            let mut request = self.bounded_operator.lister_with(&key);
            if self.native_capabilities().list_with_limit {
                request = request.limit(1);
            }
            let mut entries = request.await.map_err(error::map)?;
            match entries.next().await {
                Some(Ok(_)) => return Ok(metadata(reference, true, None)),
                Some(Err(e)) => return Err(error::map(e)),
                None => {
                    let value = self.bounded_operator.stat(&key).await.map_err(error::map)?;
                    if value.is_dir() {
                        return Ok(metadata(reference, true, None));
                    }
                    return Err(Error::new(ErrorKind::NotFound, "object prefix not found"));
                }
            }
        }
        let value = self.bounded_operator.stat(&key).await.map_err(error::map)?;
        let mut result = metadata(reference, value.is_dir(), Some(value.content_length()));
        result.modified = value.last_modified().map(|value| value.to_string());
        Ok(result)
    }
    pub(crate) async fn children(&self, reference: &str) -> Result<Vec<ObjectMetadata>> {
        if reference != "/" && !reference.ends_with('/') {
            return Err(crate::invalid());
        }
        let relative = directory(reference)?;
        let key = self.key(reference)?;
        let mut request = self.bounded_operator.lister_with(&key).recursive(false);
        if self.native_capabilities().list_with_limit {
            request = request.limit(100);
        }
        let mut lister = request.await.map_err(error::map)?;
        let mut entries = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut bytes = 0;
        let mut scanned = 0;
        while let Some(entry) = lister.next().await {
            let entry = entry.map_err(error::map)?;
            scanned += 1;
            if scanned > ENTRY_LIMIT + 1 {
                return Err(limit());
            }
            let path = entry
                .path()
                .strip_prefix(&self.prefix)
                .ok_or_else(|| Error::new(ErrorKind::Protocol, "object listing escaped root"))?;
            if path == relative {
                continue;
            }
            let child = path.strip_prefix(&relative).ok_or_else(|| {
                Error::new(ErrorKind::Protocol, "object listing escaped directory")
            })?;
            if child.trim_end_matches('/').contains('/') || child.is_empty() {
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "non-child in object listing",
                ));
            }
            if child.starts_with(".waybill-") {
                continue;
            }
            validate_path(path)
                .map_err(|_| Error::new(ErrorKind::Protocol, "invalid listed object key"))?;
            if !seen.insert(path.to_owned()) {
                continue;
            }
            let meta = entry.metadata();
            let mut object = metadata(path, meta.is_dir(), Some(meta.content_length()));
            object.modified = meta.last_modified().map(|value| value.to_string());
            bytes += object.reference.len()
                + object.name.len()
                + object.modified.as_ref().map_or(0, String::len);
            if bytes > METADATA_LIMIT || entries.len() == ENTRY_LIMIT {
                return Err(limit());
            }
            entries.push(object);
        }
        Ok(entries)
    }
}
fn metadata(reference: &str, directory: bool, size: Option<u64>) -> ObjectMetadata {
    ObjectMetadata {
        reference: reference.into(),
        name: reference
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .into(),
        kind: if directory {
            ObjectKind::Directory
        } else {
            ObjectKind::File
        },
        size: if directory { None } else { size },
        modified: None,
    }
}
fn limit() -> Error {
    Error::new(ErrorKind::Protocol, "object listing exceeds bound")
}
