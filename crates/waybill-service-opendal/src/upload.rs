//! 随机临时对象、内容读回与条件发布；不恢复 OpenDAL 私有 multipart 会话。
use crate::{
    ObjectStorage,
    download::{changed, revision},
    error,
};
use futures_util::StreamExt;
use opendal::Metadata;
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
pub(crate) mod marker;
mod state;
use state::{Phase, State, suffixed};
impl ObjectStorage {
    fn upload_state(
        &self,
        intent: &UploadIntent,
        source: &SourceIdentity,
        saved: &DriverState,
    ) -> Result<State> {
        let state = State::decode(&self.identity, intent, source, saved)?;
        self.key(&state.destination)?;
        Ok(state)
    }
    async fn optional_stat(&self, key: &str) -> Result<Option<Metadata>> {
        match self.bounded_operator.stat(key).await {
            Ok(meta) => Ok(Some(meta)),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(error::map(e)),
        }
    }
    async fn verify_object(
        &self,
        reference: &str,
        key: String,
        source: &SourceIdentity,
        expected: &Metadata,
        marker: &str,
        limit: usize,
    ) -> Result<()> {
        if limit == 0 {
            return Err(crate::invalid());
        }
        let media = self.media(reference, key.clone()).await?;
        let limit = limit / media.read_buffer_multiplier();
        if limit == 0 {
            return Err(crate::invalid());
        }
        let before = media.identity().await?;
        if before.size != source.size || before.revision != revision(expected)? {
            return Err(changed());
        }
        let mut offset = 0;
        let mut hash = blake3::Hasher::new();
        while offset < source.size {
            let count =
                (source.size - offset).min(limit.min(media.max_read_size()) as u64) as usize;
            hash.update(&media.read_range(offset, count).await?);
            offset += count as u64;
        }
        if hash.finalize().to_hex().as_str() != source.blake3 || media.identity().await? != before {
            return Err(changed());
        }
        let after = self.bounded_operator.stat(&key).await.map_err(error::map)?;
        if revision(&after)? != before.revision
            || after.version() != expected.version()
            || !marker::owned(&self.bounded_operator, &after, marker)
        {
            return Err(changed());
        }
        Ok(())
    }

    async fn write_body(
        &self,
        key: &str,
        marker: String,
        expected_size: u64,
        mut body: UploadBody,
    ) -> Result<()> {
        let mut writer =
            marker::writer(&self.bounded_operator, key, marker, self.write_buffer).await?;
        let result = async {
            let mut total = 0u64;
            while let Some(data) = body.next().await {
                let data = data?;
                total = total
                    .checked_add(data.len() as u64)
                    .ok_or_else(crate::invalid)?;
                if total > expected_size {
                    return Err(crate::invalid());
                }
                writer.write(data).await.map_err(error::write)?;
            }
            if total != expected_size {
                return Err(Error::new(ErrorKind::Protocol, "short object upload body"));
            }
            writer.close().await.map_err(error::write)?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = writer.abort().await;
        }
        result
    }
    async fn completed_destination(
        &self,
        intent: &UploadIntent,
        source: &SourceIdentity,
        state: &State,
        read_limit: usize,
    ) -> Result<Option<StreamStatus>> {
        let final_key = self.key(&state.destination)?;
        if let Some(meta) = self.optional_stat(&final_key).await? {
            if !marker::owned(&self.bounded_operator, &meta, &state.marker())
                || matches!(state.phase, Phase::Prepared | Phase::Writing)
            {
                return Err(conflict());
            }
            self.verify_object(
                &state.destination,
                final_key,
                source,
                &meta,
                &state.marker(),
                read_limit,
            )
            .await?;
            self.cleanup_staging(state).await;
            return Ok(Some(StreamStatus::Complete {
                state: state.encode()?,
                receipt: Receipt {
                    operation: intent.operation.clone(),
                    service: self.identity.clone(),
                    target: intent.target.clone(),
                    object: state.destination.clone(),
                    size: source.size,
                    verified: Verification::Digest {
                        algorithm: DigestAlgorithm::Blake3,
                        value: source.blake3.clone(),
                    },
                },
            }));
        }
        Ok(None)
    }

    async fn cleanup_staging(&self, state: &State) {
        // 最终内容已验证；只在后端支持版本条件删除时回收当前操作的暂存。
        // 回收失败不改变交付结果，遗留对象由宿主 / bucket 生命周期处理。
        if self.native_capabilities().delete_with_if_match {
            let temporary = format!("{}{}", self.prefix, state.temporary);
            if let Ok(Some(meta)) = self.optional_stat(&temporary).await
                && marker::owned(&self.bounded_operator, &meta, &state.marker())
                && let Ok(etag) = revision(&meta)
            {
                let _ = self
                    .bounded_operator
                    .delete_with(&temporary)
                    .if_match(&etag)
                    .await;
            }
        }
    }

    async fn staging_status(
        &self,
        source: &SourceIdentity,
        mut state: State,
        read_limit: usize,
    ) -> Result<StreamStatus> {
        let key = format!("{}{}", self.prefix, state.temporary);
        if let Some(meta) = self.optional_stat(&key).await? {
            if !marker::owned(&self.bounded_operator, &meta, &state.marker())
                || matches!(state.phase, Phase::Prepared)
            {
                return Err(conflict());
            }
            let etag = revision(&meta)?;
            if let Phase::Staged { etag: expected, .. } = &state.phase
                && expected != &etag
            {
                return Err(changed());
            }
            if meta.content_length() == source.size {
                self.verify_object(
                    &state.temporary,
                    key,
                    source,
                    &meta,
                    &state.marker(),
                    read_limit,
                )
                .await?;
                state.phase = Phase::Staged {
                    etag,
                    version: meta.version().map(str::to_owned),
                    read_limit: (read_limit / 2).min(8 * 1024 * 1024),
                };
                return Ok(StreamStatus::Staged(state.encode()?));
            }
            if matches!(state.phase, Phase::Staged { .. }) {
                return Err(changed());
            }
        } else if matches!(state.phase, Phase::Staged { .. }) {
            return Err(Error::new(
                ErrorKind::ResultUnknown,
                "verified object staging missing",
            ));
        }
        let restart_required = !matches!(state.phase, Phase::Prepared);
        Ok(StreamStatus::Ready {
            state: state.encode()?,
            restart_required,
        })
    }
}
impl StreamUploadSink for ObjectStorage {
    fn write_buffer_size(&self) -> usize {
        self.write_buffer * 2
    }
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
            self.key(&intent.target)?;
            let destination = if intent.conflict == ConflictPolicy::OperationSuffix
                && self
                    .optional_stat(&self.key(&intent.target)?)
                    .await?
                    .is_some()
            {
                suffixed(&intent.target, &intent.operation)
            } else {
                intent.target.clone()
            };
            self.key(&destination)?;
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
            if read_limit == 0 {
                return Err(crate::invalid());
            }
            let state = self.upload_state(intent, source, saved)?;
            if let Some(complete) = self
                .completed_destination(intent, source, &state, read_limit)
                .await?
            {
                return Ok(complete);
            }
            self.staging_status(source, state, read_limit).await
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
            if matches!(state.phase, Phase::Staged { .. }) {
                return Err(Error::new(
                    ErrorKind::Checkpoint,
                    "verified staging cannot restart",
                ));
            }
            if self
                .optional_stat(&self.key(&state.destination)?)
                .await?
                .is_some()
            {
                return Err(conflict());
            }
            // 新尝试使用新 key，避免覆盖无法核验的旧临时对象。
            state.restart()?;
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
            if !matches!(state.phase, Phase::Writing) {
                return Err(Error::new(
                    ErrorKind::Checkpoint,
                    "object write intent missing",
                ));
            }
            let key = format!("{}{}", self.prefix, state.temporary);
            self.write_body(&key, state.marker(), source.size, body)
                .await
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
            let Phase::Staged {
                etag,
                version,
                read_limit,
            } = &state.phase
            else {
                return Err(Error::new(
                    ErrorKind::Checkpoint,
                    "object staging not verified",
                ));
            };
            let from = format!("{}{}", self.prefix, state.temporary);
            let to = self.key(&state.destination)?;
            let meta = self
                .bounded_operator
                .stat(&from)
                .await
                .map_err(error::map)?;
            if revision(&meta)? != *etag
                || !marker::owned(&self.bounded_operator, &meta, &state.marker())
            {
                return Err(changed());
            }
            let caps = self.native_capabilities();
            // 目标条件创建不能冻结复制源；没有稳定源版本时必须使用条件读回。
            if caps.copy
                && caps.copy_with_if_not_exists
                && caps.copy_with_source_version
                && let Some(version) = version.as_deref().filter(|v| *v != "null")
            {
                let copy = self
                    .bounded_operator
                    .copy_with(&from, &to)
                    .if_not_exists(true)
                    .concurrent(1)
                    .source_version(version);
                copy.await.map_err(error::write)?;
            } else {
                // OSS 等后端未公开条件 copy；逐块条件读取，再以条件完整写入发布。
                let media = self.media(&state.temporary, from).await?;
                if media.identity().await?.revision != *etag {
                    return Err(changed());
                }
                let expected = etag.clone();
                let size = source.size;
                let limit = *read_limit;
                let body = Box::pin(futures_util::stream::try_unfold(
                    (media, 0u64),
                    move |(media, offset)| {
                        let expected = expected.clone();
                        async move {
                            if offset == size {
                                if media.identity().await?.revision != expected {
                                    return Err(changed());
                                }
                                return Ok(None);
                            }
                            let count = (size - offset).min(limit as u64) as usize;
                            let data = media.read_range(offset, count).await?;
                            Ok(Some((data, (media, offset + count as u64))))
                        }
                    },
                ));
                self.write_body(&to, state.marker(), source.size, body)
                    .await?;
            }
            // 完成对账由 probe 执行；这里不删除暂存，避免响应丢失后失去恢复依据。
            Ok(())
        })
    }
}
fn conflict() -> Error {
    Error::new(
        ErrorKind::Conflict,
        "object target belongs to another operation",
    )
}
