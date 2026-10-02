//! 普通 PUT 整文件写入；随机暂存、读取校验和条件 MOVE 分开处理。
use crate::{
    Webdav,
    catalog::Metadata,
    client::{network, success},
};
use reqwest::Method;
use waybill::{
    BoxFuture,
    checkpoint::DriverState,
    content::{DigestAlgorithm, Verification},
    error::{Error, ErrorKind, Result},
    service::{Capabilities, Service, ServiceIdentity},
    source::SourceIdentity,
    transfer::{ConflictPolicy, Receipt},
    upload::{StreamStatus, StreamUploadSink, UploadBody, UploadIntent},
};

mod state;
use state::{Phase, State, invalid_state, suffixed};

impl Webdav {
    fn upload_state(
        &self,
        intent: &UploadIntent,
        source: &SourceIdentity,
        saved: &DriverState,
    ) -> Result<State> {
        let state = State::decode(&self.identity, intent, source, saved)?;
        self.api.paths.url(&state.destination)?;
        self.api.paths.url(&state.temporary)?;
        Ok(state)
    }
    async fn optional_stat(&self, reference: &str) -> Result<Option<Metadata>> {
        match self.stat(reference).await {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    async fn stable_metadata(&self, reference: &str, mut metadata: Metadata) -> Result<Metadata> {
        // Apache 刚写入的文件短暂返回弱 ETag；仅有界等待强版本，不降低校验要求。
        for attempt in 0..=3 {
            if metadata
                .etag
                .as_deref()
                .is_some_and(crate::download::strong_etag)
            {
                let (size, etag) = self.head_identity(reference).await?;
                if metadata.object.size != Some(size)
                    || metadata.etag.as_deref() != Some(etag.as_str())
                {
                    return Err(changed());
                }
                return Ok(metadata);
            }
            if attempt == 3
                || !metadata
                    .etag
                    .as_deref()
                    .is_some_and(|tag| tag.starts_with("W/"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            metadata = self.stat(reference).await?;
        }
        Err(Error::new(
            ErrorKind::Unsupported,
            "WebDAV temporary object needs strong ETag",
        ))
    }
    async fn verify_upload(
        &self,
        reference: &str,
        source: &SourceIdentity,
        read_limit: usize,
    ) -> Result<String> {
        if read_limit == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "zero verification read limit",
            ));
        }
        let media = self.open_media(reference).await?;
        let before = media.identity().await?;
        if before.size != source.size {
            return Err(Error::new(
                ErrorKind::SourceChanged,
                "WebDAV upload length mismatch",
            ));
        }
        let chunk_size = media.max_read_size().min(read_limit);
        let mut hash = blake3::Hasher::new();
        let mut offset = 0;
        while offset < before.size {
            let length = (before.size - offset).min(chunk_size as u64) as usize;
            let data = media.read_range(offset, length).await?;
            hash.update(&data);
            offset += length as u64;
        }
        if hash.finalize().to_hex().as_str() != source.blake3 {
            return Err(Error::new(
                ErrorKind::SourceChanged,
                "WebDAV upload content mismatch",
            ));
        }
        if media.identity().await? != before {
            return Err(Error::new(
                ErrorKind::SourceChanged,
                "WebDAV upload changed during verification",
            ));
        }
        Ok(before.revision)
    }
    async fn create_parents(&self, destination: &str) -> Result<()> {
        let Some((parent, _)) = destination.rsplit_once('/') else {
            return Ok(());
        };
        let mut reference = String::new();
        for segment in parent.split('/') {
            reference.push_str(segment);
            reference.push('/');
            if let Some(existing) = self.optional_stat(&reference).await? {
                if !existing.object.is_directory() {
                    return Err(conflict());
                }
                continue;
            }
            let response = self
                .api
                .request(method(b"MKCOL")?, &reference)
                .await?
                .send()
                .await
                .map_err(network)?;
            if !response.status().is_success() {
                // 并发创建或响应丢失时只接受已确认的目录，不把 405 当成功。
                if !self
                    .optional_stat(&reference)
                    .await?
                    .is_some_and(|m| m.object.is_directory())
                {
                    success(&response)?;
                    return Err(conflict());
                }
            }
        }
        Ok(())
    }
}
impl StreamUploadSink for Webdav {
    fn identity(&self) -> ServiceIdentity {
        self.identity.clone()
    }
    fn capabilities(&self) -> Capabilities {
        self.info().capabilities
    }
    fn prepare<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async move {
            if !waybill::object::valid_object_path(&intent.target) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "WebDAV upload requires relative file path",
                ));
            }
            self.api.paths.url(&intent.target)?;
            let destination = if intent.conflict == ConflictPolicy::OperationSuffix
                && self.optional_stat(&intent.target).await?.is_some()
            {
                suffixed(&intent.target, &intent.operation)
            } else {
                intent.target.clone()
            };
            self.api.paths.url(&destination)?;
            State::new(&self.identity, intent, source, destination)?.encode()
        })
    }
    fn probe<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        saved: &'a DriverState,
        read_limit: usize,
    ) -> BoxFuture<'a, StreamStatus> {
        Box::pin(async move {
            let mut state = self.upload_state(intent, source, saved)?;
            if let Some(final_object) = self.optional_stat(&state.destination).await? {
                if final_object.object.is_directory()
                    || final_object.delivery.as_deref() != Some(state.marker().as_str())
                {
                    return Err(conflict());
                }
                self.stable_metadata(&state.destination, final_object)
                    .await?;
                self.verify_upload(&state.destination, source, read_limit)
                    .await?;
                let receipt = Receipt {
                    operation: intent.operation.clone(),
                    service: self.identity.clone(),
                    target: intent.target.clone(),
                    object: state.destination.clone(),
                    size: source.size,
                    verified: Verification::Digest {
                        algorithm: DigestAlgorithm::Blake3,
                        value: source.blake3.clone(),
                    },
                };
                return Ok(StreamStatus::Complete {
                    state: state.encode()?,
                    receipt,
                });
            }
            if let Some(staged) = self.optional_stat(&state.temporary).await? {
                if matches!(state.phase, Phase::Prepared)
                    || staged.object.is_directory()
                    || staged
                        .delivery
                        .as_ref()
                        .is_some_and(|marker| marker != &state.marker())
                {
                    return Err(conflict());
                }
                let staged = self.stable_metadata(&state.temporary, staged).await?;
                let etag = staged
                    .etag
                    .filter(|tag| crate::download::strong_etag(tag))
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::Unsupported,
                            "WebDAV temporary object needs strong ETag",
                        )
                    })?;
                if staged.object.size == Some(source.size) {
                    // 长度相同仍须读取验证；失败禁止用 PUT 覆盖未知内容。
                    state.phase = Phase::Staged {
                        etag: self
                            .verify_upload(&state.temporary, source, read_limit)
                            .await?,
                    };
                    return Ok(StreamStatus::Staged(state.encode()?));
                }
                if matches!(state.phase, Phase::Staged { .. }) {
                    return Err(Error::new(
                        ErrorKind::SourceChanged,
                        "verified WebDAV temporary object changed",
                    ));
                }
                state.phase = Phase::Writing { etag: Some(etag) };
            } else {
                if matches!(state.phase, Phase::Staged { .. }) {
                    return Err(Error::new(
                        ErrorKind::ResultUnknown,
                        "WebDAV publish object unavailable",
                    ));
                }
                if matches!(state.phase, Phase::Writing { .. }) {
                    state.phase = Phase::Writing { etag: None };
                }
            }
            Ok(StreamStatus::Ready {
                restart_required: !matches!(state.phase, Phase::Prepared),
                state: state.encode()?,
            })
        })
    }
    fn begin<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        saved: &'a DriverState,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async move {
            let mut state = self.upload_state(intent, source, saved)?;
            state.phase = match state.phase {
                Phase::Prepared => Phase::Writing { etag: None },
                Phase::Writing { etag } => Phase::Writing { etag },
                Phase::Staged { .. } => return Err(invalid_state()),
            };
            state.encode()
        })
    }
    fn write<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        saved: &'a DriverState,
        body: UploadBody,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let state = self.upload_state(intent, source, saved)?;
            let Phase::Writing { etag } = &state.phase else {
                return Err(invalid_state());
            };
            self.create_parents(&state.destination).await?;
            let request = self
                .api
                .request(Method::PUT, &state.temporary)
                .await?
                .header("Content-Length", source.size)
                .body(reqwest::Body::wrap_stream(body));
            let request = match etag {
                Some(tag) => request.header("If-Match", tag),
                None => request.header("If-None-Match", "*"),
            };
            let response = request.send().await.map_err(network)?;
            success(&response)
        })
    }
    fn publish<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        saved: &'a DriverState,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let state = self.upload_state(intent, source, saved)?;
            let Phase::Staged { etag } = &state.phase else {
                return Err(invalid_state());
            };
            let etag = etag.as_str();
            let before = self.stat(&state.temporary).await?;
            if before.etag.as_deref() != Some(etag) {
                return Err(changed());
            }
            if before.delivery.as_deref() != Some(state.marker().as_str()) {
                if before.delivery.is_some() {
                    return Err(conflict());
                }
                let xml = format!(
                    r#"<d:propertyupdate xmlns:d="DAV:" xmlns:w="urn:waybill"><d:set><d:prop><w:delivery>{}</w:delivery></d:prop></d:set></d:propertyupdate>"#,
                    state.marker()
                );
                let response = self
                    .api
                    .request(method(b"PROPPATCH")?, &state.temporary)
                    .await?
                    .header("If-Match", etag)
                    .header("Content-Type", "application/xml; charset=utf-8")
                    .body(xml)
                    .send()
                    .await
                    .map_err(network)?;
                success(&response)?;
                let marked = self.stat(&state.temporary).await?;
                if marked.delivery.as_deref() != Some(state.marker().as_str()) {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "WebDAV operation properties required for publish reconciliation",
                    ));
                }
                if marked.etag.as_deref() != Some(etag) {
                    return Err(changed());
                }
            }
            let destination = self.api.paths.url(&state.destination)?;
            let response = self
                .api
                .request(method(b"MOVE")?, &state.temporary)
                .await?
                .header("Destination", destination.as_str())
                .header("Overwrite", "F")
                .header("If-Match", etag)
                .send()
                .await
                .map_err(network)?;
            success(&response)
        })
    }
}
fn conflict() -> Error {
    Error::new(
        ErrorKind::Conflict,
        "WebDAV target belongs to another operation",
    )
}
fn changed() -> Error {
    Error::new(
        ErrorKind::SourceChanged,
        "WebDAV temporary object changed before publish",
    )
}
fn method(value: &[u8]) -> Result<Method> {
    Method::from_bytes(value).map_err(|_| Error::new(ErrorKind::Protocol, "invalid WebDAV method"))
}

#[cfg(test)]
mod tests;
