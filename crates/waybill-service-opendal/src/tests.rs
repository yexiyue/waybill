//! S3 HTTP 替身验证恢复窗口和协议约束；真实对象存储验收见 tests/object-storage。
use super::*;
use std::{collections::BTreeMap, sync::Mutex};
use waybill::{
    error::ErrorKind,
    service::Service,
    source::SourceIdentity,
    transfer::ConflictPolicy,
    upload::{StreamStatus, StreamUploadSink, UploadIntent, UploadOptions},
};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
#[derive(Clone)]
struct Blob {
    bytes: Vec<u8>,
    marker: String,
    etag: String,
}
#[derive(Clone, Default)]
struct Store {
    blobs: Arc<Mutex<BTreeMap<String, Blob>>>,
    lost_copy_response: Arc<std::sync::atomic::AtomicBool>,
    ignore_range: Arc<std::sync::atomic::AtomicBool>,
    versioned: Arc<std::sync::atomic::AtomicBool>,
    replace_after_head: Arc<Mutex<Option<(String, Blob)>>>,
}
impl Store {
    fn put(&self, key: &str, bytes: &[u8], marker: &str) {
        self.blobs.lock().unwrap().insert(
            key.into(),
            Blob {
                bytes: bytes.into(),
                marker: marker.into(),
                etag: format!("\"{}\"", blake3::hash(bytes).to_hex()),
            },
        );
    }
}
fn h<'a>(request: &'a Request, key: &str) -> Option<&'a str> {
    request.headers.get(key).and_then(|v| v.to_str().ok())
}
impl Respond for Store {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        use std::sync::atomic::Ordering;
        let key = request.url.path().strip_prefix("/test/").unwrap_or("");
        let mut blobs = self.blobs.lock().unwrap();
        match request.method.as_str() {
            "HEAD" => {
                let response = match blobs.get(key) {
                    Some(blob) => {
                        let response = ResponseTemplate::new(200)
                            .insert_header("content-length", blob.bytes.len().to_string())
                            .insert_header("etag", &blob.etag)
                            .insert_header("x-amz-meta-waybill-delivery", &blob.marker);
                        if self.versioned.load(Ordering::Relaxed) {
                            response.insert_header("x-amz-version-id", "verified-version")
                        } else {
                            response
                        }
                    }
                    None => ResponseTemplate::new(404),
                };
                let mut replacement = self.replace_after_head.lock().unwrap();
                if replacement
                    .as_ref()
                    .is_some_and(|(target, _)| target == key)
                {
                    let (target, blob) = replacement.take().unwrap();
                    blobs.insert(target, blob);
                }
                response
            }
            "GET" if request.url.query_pairs().any(|(k, _)| k == "list-type") => {
                let prefix = request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "prefix")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let mut xml = String::from("<ListBucketResult><IsTruncated>false</IsTruncated>");
                let mut directories = std::collections::BTreeSet::new();
                for (key, blob) in blobs.iter().filter(|(key, _)| key.starts_with(&prefix)) {
                    let child = &key[prefix.len()..];
                    if let Some((dir, _)) = child.split_once('/') {
                        let dir = format!("{prefix}{dir}/");
                        if directories.insert(dir.clone()) {
                            xml.push_str(&format!(
                                "<CommonPrefixes><Prefix>{dir}</Prefix></CommonPrefixes>"
                            ));
                        }
                    } else {
                        xml.push_str(&format!("<Contents><Key>{key}</Key><Size>{}</Size><LastModified>2026-10-02T00:00:00Z</LastModified><ETag>{}</ETag></Contents>", blob.bytes.len(), blob.etag));
                    }
                }
                xml.push_str("</ListBucketResult>");
                ResponseTemplate::new(200).set_body_string(xml)
            }
            "GET" => match blobs.get(key) {
                None => ResponseTemplate::new(404),
                Some(blob) if h(request, "if-match").is_some_and(|tag| tag != blob.etag) => {
                    ResponseTemplate::new(412)
                }
                Some(blob)
                    if h(request, "range").is_none()
                        || self.ignore_range.load(Ordering::Relaxed) =>
                {
                    ResponseTemplate::new(200)
                        .insert_header("etag", &blob.etag)
                        .set_body_bytes(blob.bytes.clone())
                }
                Some(blob) => {
                    let (start, end) = h(request, "range")
                        .unwrap()
                        .strip_prefix("bytes=")
                        .unwrap()
                        .split_once('-')
                        .unwrap();
                    let start: usize = start.parse().unwrap();
                    let end: usize = end.parse().unwrap();
                    ResponseTemplate::new(206)
                        .insert_header("etag", &blob.etag)
                        .insert_header(
                            "content-range",
                            format!("bytes {start}-{end}/{}", blob.bytes.len()),
                        )
                        .set_body_bytes(blob.bytes[start..=end].to_vec())
                }
            },
            "PUT" if h(request, "x-amz-copy-source").is_some() => {
                if h(request, "if-none-match") != Some("*") || blobs.contains_key(key) {
                    return ResponseTemplate::new(412);
                }
                let from = h(request, "x-amz-copy-source")
                    .unwrap()
                    .trim_start_matches('/')
                    .strip_prefix("test/")
                    .unwrap();
                let (from, version) = from
                    .split_once('?')
                    .map_or((from, None), |(key, query)| (key, Some(query)));
                if self.versioned.load(Ordering::Relaxed)
                    && version != Some("versionId=verified-version")
                {
                    return ResponseTemplate::new(412);
                }
                let blob = blobs.get(from).unwrap().clone();
                let etag = blob.etag.clone();
                blobs.insert(key.into(), blob);
                if self.lost_copy_response.swap(false, Ordering::Relaxed) {
                    return ResponseTemplate::new(503)
                        .set_body_string("<Error><Code>SlowDown</Code></Error>");
                }
                ResponseTemplate::new(200).set_body_string(format!("<CopyObjectResult><ETag>{etag}</ETag><LastModified>2026-10-02T00:00:00Z</LastModified></CopyObjectResult>"))
            }
            "PUT" => {
                if h(request, "if-none-match") != Some("*") || blobs.contains_key(key) {
                    return ResponseTemplate::new(412);
                }
                let marker = h(request, "x-amz-meta-waybill-delivery")
                    .unwrap_or("")
                    .to_owned();
                let etag = format!("\"{}\"", blake3::hash(&request.body).to_hex());
                blobs.insert(
                    key.into(),
                    Blob {
                        bytes: request.body.clone(),
                        marker,
                        etag: etag.clone(),
                    },
                );
                ResponseTemplate::new(200).insert_header("etag", etag)
            }
            "DELETE" => {
                if let Some(blob) = blobs.get(key)
                    && h(request, "if-match").is_some_and(|tag| tag != blob.etag)
                {
                    return ResponseTemplate::new(412);
                }
                blobs.remove(key);
                ResponseTemplate::new(204)
            }
            _ => ResponseTemplate::new(500),
        }
    }
}
async fn fixture() -> (MockServer, Store, ObjectStorage) {
    let server = MockServer::start().await;
    let store = Store::default();
    Mock::given(wiremock::matchers::any())
        .respond_with(store.clone())
        .mount(&server)
        .await;
    let operator = Operator::via_iter(
        "s3",
        [
            ("bucket".into(), "test".into()),
            ("region".into(), "us-east-1".into()),
            ("endpoint".into(), server.uri()),
            ("access_key_id".into(), "test".into()),
            ("secret_access_key".into(), "test-secret".into()),
            ("disable_config_load".into(), "true".into()),
        ],
    )
    .unwrap();
    let mut config = ObjectStorageConfig::new("test-principal");
    config.conditional_writes = true;
    let service = ObjectStorage::new(operator, config).unwrap();
    (server, store, service)
}
#[tokio::test]
async fn listing_and_ranges_are_confined_to_selected_root() {
    let (_server, store, service) = fixture().await;
    store.put("root/a.bin", b"abcd", "");
    store.put("root/sub/b.bin", b"other", "");
    store.put("elsewhere", b"outside", "");
    store.put("root/.waybill-secret.part", b"internal", "");
    let selected = service.at_root("root/").unwrap();
    let entries = selected.list("/").await.unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|e| e.reference == "a.bin" || e.reference == "sub/")
    );
    let source = selected.download_source("a.bin").await.unwrap();
    assert_eq!(source.read_range(1, 2).await.unwrap(), b"bc");
    assert_ne!(selected.info().identity, service.info().identity);
    for bad in ["../a", "a//b", "/a", ".waybill-hidden.part", "a\\b"] {
        assert!(selected.resolve(bad).await.is_err());
    }
}
#[tokio::test]
async fn changed_source_and_ignored_ranges_are_rejected() {
    let (_server, store, service) = fixture().await;
    store.put("a", b"abcd", "");
    let source = service.download_source("a").await.unwrap();
    store.put("a", b"changed", "");
    assert_eq!(
        source.identity().await.unwrap_err().kind,
        ErrorKind::SourceChanged
    );
    assert_eq!(
        source.read_range(0, 1).await.unwrap_err().kind,
        ErrorKind::SourceChanged
    );
    store
        .ignore_range
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(service.download_source("a").await.is_err());
}
#[tokio::test]
async fn upload_reconciles_lost_publish_response_and_reuses_receipt() {
    let (_server, store, service) = fixture().await;
    store
        .versioned
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source"), b"cloud delivery").unwrap();
    let source = Arc::new(
        waybill_service_fs::FileSource::open(dir.path().join("source"))
            .await
            .unwrap(),
    );
    let engine = waybill::transfer::TransferEngine::new(
        waybill_service_fs::FileCheckpointStore::new(dir.path().join("checkpoints")),
    );
    store
        .lost_copy_response
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let intent = UploadIntent {
        operation: "test-upload".into(),
        target: "output.bin".into(),
        conflict: ConflictPolicy::Reject,
    };
    let first = engine
        .upload_stream(
            source.clone(),
            &service,
            UploadOptions {
                intent: intent.clone(),
                policy: Default::default(),
                stop: Default::default(),
                progress: None,
            },
        )
        .await
        .unwrap();
    let second = engine
        .upload_stream(
            source,
            &service,
            UploadOptions {
                intent,
                policy: Default::default(),
                stop: Default::default(),
                progress: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(
        store.blobs.lock().unwrap()["output.bin"].bytes,
        b"cloud delivery"
    );
    assert_eq!(store.blobs.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn conflicting_targets_are_not_overwritten() {
    let (_server, store, service) = fixture().await;
    store.put("output.bin", b"existing", "someone-else");
    let source = SourceIdentity {
        size: 3,
        blake3: blake3::hash(b"new").to_hex().to_string(),
        reference: "local".into(),
        revision: "v1".into(),
    };
    let intent = UploadIntent {
        operation: "conflict".into(),
        target: "output.bin".into(),
        conflict: ConflictPolicy::Reject,
    };
    let state = service.prepare(&intent, &source).await.unwrap();
    assert_eq!(
        service
            .probe(&intent, &source, &state, 1024)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(store.blobs.lock().unwrap()["output.bin"].bytes, b"existing");
}
#[tokio::test]
async fn unfinished_upload_requires_explicit_restart_and_state_is_bound() {
    let (_server, _store, service) = fixture().await;
    let source = SourceIdentity {
        size: 3,
        blake3: blake3::hash(b"new").to_hex().to_string(),
        reference: "local".into(),
        revision: "v1".into(),
    };
    let intent = UploadIntent {
        operation: "restart".into(),
        target: "out".into(),
        conflict: ConflictPolicy::Reject,
    };
    let prepared = service.prepare(&intent, &source).await.unwrap();
    let writing = service.begin(&intent, &source, &prepared).await.unwrap();
    assert!(matches!(
        service
            .probe(&intent, &source, &writing, 1024)
            .await
            .unwrap(),
        StreamStatus::Ready {
            restart_required: true,
            ..
        }
    ));
    let other = service.at_root("other/").unwrap();
    assert_eq!(
        other
            .probe(&intent, &source, &writing, 1024)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Checkpoint
    );
}
#[tokio::test]
async fn upload_budget_is_rejected_before_remote_creation() {
    let (_server, store, service) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source"), b"data").unwrap();
    let source = Arc::new(
        waybill_service_fs::FileSource::open(dir.path().join("source"))
            .await
            .unwrap(),
    );
    let engine = waybill::transfer::TransferEngine::new(
        waybill_service_fs::FileCheckpointStore::new(dir.path().join("checkpoints")),
    )
    .with_budget(Arc::new(
        waybill::budget::ResourceBudget::new(1024, 1).unwrap(),
    ));
    let intent = UploadIntent {
        operation: "budget".into(),
        target: "out".into(),
        conflict: ConflictPolicy::Reject,
    };
    assert_eq!(
        engine
            .upload_stream(
                source,
                &service,
                UploadOptions {
                    intent,
                    policy: Default::default(),
                    stop: Default::default(),
                    progress: None
                }
            )
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidInput
    );
    assert!(store.blobs.lock().unwrap().is_empty());
}
#[test]
fn all_enabled_backends_build_and_readonly_config_does_not_claim_upload() {
    for (_, backend) in [
        (cfg!(feature = "s3"), "s3"),
        (cfg!(feature = "oss"), "oss"),
        (cfg!(feature = "cos"), "cos"),
        (cfg!(feature = "obs"), "obs"),
        (cfg!(feature = "tos"), "tos"),
        (cfg!(feature = "gcs"), "gcs"),
        (cfg!(feature = "azblob"), "azblob"),
        (cfg!(feature = "b2"), "b2"),
        (cfg!(feature = "swift"), "swift"),
        (cfg!(feature = "upyun"), "upyun"),
        (cfg!(feature = "vercel-blob"), "vercel-blob"),
    ]
    .into_iter()
    .filter(|(enabled, _)| *enabled)
    {
        // 构造不代表服务端通过验收；至少确认各 feature 已注册，而非 Unsupported scheme。
        let error = Operator::via_iter(backend, []).err();
        if let Some(error) = error {
            assert_ne!(error.kind(), opendal::ErrorKind::Unsupported, "{backend}");
        }
    }
    let op = Operator::new(opendal::services::Memory::default()).unwrap();
    let service = ObjectStorage::new(op, ObjectStorageConfig::new("memory")).unwrap();
    assert!(!service.info().capabilities.stream_upload);
    assert!(service.stream_upload_sink().is_err());
}

#[tokio::test]
async fn conditional_writer_fallback_publishes_without_copy() {
    let (_server, store, original) = fixture().await;
    let operator = original
        .operator
        .clone()
        .layer(opendal::layers::CapabilityOverrideLayer::new(|mut c| {
            c.copy_with_if_not_exists = false;
            c
        }));
    let service = ObjectStorage::new(operator, original.config.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source"), b"fallback").unwrap();
    let source = Arc::new(
        waybill_service_fs::FileSource::open(dir.path().join("source"))
            .await
            .unwrap(),
    );
    let engine = waybill::transfer::TransferEngine::new(
        waybill_service_fs::FileCheckpointStore::new(dir.path().join("checkpoints")),
    );
    let receipt = engine
        .upload_stream(source, &service, UploadOptions::new("fallback", "out"))
        .await
        .unwrap();
    assert_eq!(receipt.size, 8);
    assert_eq!(store.blobs.lock().unwrap()["out"].bytes, b"fallback");
}

#[tokio::test]
async fn empty_objects_and_operation_suffix_keep_existing_data() {
    let (_server, store, service) = fixture().await;
    store.put("out", b"existing", "someone-else");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source"), b"").unwrap();
    let source = Arc::new(
        waybill_service_fs::FileSource::open(dir.path().join("source"))
            .await
            .unwrap(),
    );
    let engine = waybill::transfer::TransferEngine::new(
        waybill_service_fs::FileCheckpointStore::new(dir.path().join("checkpoints")),
    );
    let mut options = UploadOptions::new("suffix", "out");
    options.intent.conflict = ConflictPolicy::OperationSuffix;
    let receipt = engine
        .upload_stream(source, &service, options)
        .await
        .unwrap();
    assert_ne!(receipt.object, "out");
    let blobs = store.blobs.lock().unwrap();
    assert!(blobs[&receipt.object].bytes.is_empty());
    assert_eq!(blobs["out"].bytes, b"existing");
}

#[tokio::test]
async fn metadata_body_bound_is_enforced_before_xml_parsing() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 1024 * 1024 + 1]))
        .mount(&server)
        .await;
    let operator = Operator::via_iter(
        "s3",
        [
            ("bucket".into(), "test".into()),
            ("region".into(), "us-east-1".into()),
            ("endpoint".into(), server.uri()),
            ("skip_signature".into(), "true".into()),
        ],
    )
    .unwrap();
    let service = ObjectStorage::new(operator, ObjectStorageConfig::new("test")).unwrap();
    assert_eq!(
        service.list("/").await.unwrap_err().kind,
        ErrorKind::Protocol
    );
}

#[tokio::test]
async fn publication_race_never_overwrites_competing_object() {
    for conditional_copy in [true, false] {
        let (_server, store, original) = fixture().await;
        let operator =
            original
                .operator
                .clone()
                .layer(opendal::layers::CapabilityOverrideLayer::new(
                    move |mut c| {
                        c.copy_with_if_not_exists = conditional_copy;
                        c
                    },
                ));
        let service = ObjectStorage::new(operator, original.config.clone()).unwrap();
        let source = SourceIdentity {
            size: 3,
            blake3: blake3::hash(b"new").to_hex().to_string(),
            reference: "local".into(),
            revision: "v1".into(),
        };
        let intent = UploadIntent {
            operation: "race".into(),
            target: "out".into(),
            conflict: ConflictPolicy::Reject,
        };
        let prepared = service.prepare(&intent, &source).await.unwrap();
        let writing = service.begin(&intent, &source, &prepared).await.unwrap();
        let body = Box::pin(futures_util::stream::once(async { Ok(b"new".to_vec()) }));
        service
            .write(&intent, &source, &writing, body)
            .await
            .unwrap();
        let StreamStatus::Staged(staged) = service
            .probe(&intent, &source, &writing, 1024)
            .await
            .unwrap()
        else {
            panic!("expected verified staging");
        };
        store.put("out", b"competing", "other");
        assert_eq!(
            service
                .publish(&intent, &source, &staged)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(store.blobs.lock().unwrap()["out"].bytes, b"competing");
    }
}

#[tokio::test]
async fn native_operator_access_is_separate_from_delivery_response_bounds() {
    let (_server, store, service) = fixture().await;
    let bytes = vec![42; 2 * 1024 * 1024];
    store.put("native", &bytes, "external");
    assert_eq!(
        service.operator().read("native").await.unwrap().to_vec(),
        bytes
    );
    let media = service.download_source("native").await.unwrap();
    assert_eq!(media.read_range(0, 8).await.unwrap(), vec![42; 8]);
}

#[tokio::test]
async fn completion_probe_rejects_replacement_after_ownership_check() {
    let (_server, store, service) = fixture().await;
    let source = SourceIdentity {
        size: 3,
        blake3: blake3::hash(b"new").to_hex().to_string(),
        reference: "local".into(),
        revision: "v1".into(),
    };
    let intent = UploadIntent {
        operation: "probe-replacement".into(),
        target: "out".into(),
        conflict: ConflictPolicy::Reject,
    };
    let prepared = service.prepare(&intent, &source).await.unwrap();
    let writing = service.begin(&intent, &source, &prepared).await.unwrap();
    service
        .write(
            &intent,
            &source,
            &writing,
            Box::pin(futures_util::stream::once(async { Ok(b"new".to_vec()) })),
        )
        .await
        .unwrap();
    let StreamStatus::Staged(staged) = service
        .probe(&intent, &source, &writing, 1024)
        .await
        .unwrap()
    else {
        panic!("expected verified staging");
    };
    service.publish(&intent, &source, &staged).await.unwrap();
    // 内容相同仍不能借用竞争版本的内容证明本操作归属。
    *store.replace_after_head.lock().unwrap() = Some((
        "out".into(),
        Blob {
            bytes: b"new".to_vec(),
            marker: "another-operation".into(),
            etag: "\"replacement-version\"".into(),
        },
    ));
    assert_eq!(
        service
            .probe(&intent, &source, &staged, 1024)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::SourceChanged
    );
    assert_eq!(
        store.blobs.lock().unwrap()["out"].marker,
        "another-operation"
    );
}

#[tokio::test]
async fn publication_rejects_staging_replacement_before_copy_or_read() {
    let (_server, store, service) = fixture().await;
    let source = SourceIdentity {
        size: 3,
        blake3: blake3::hash(b"new").to_hex().to_string(),
        reference: "local".into(),
        revision: "v1".into(),
    };
    let intent = UploadIntent {
        operation: "staging-replacement".into(),
        target: "out".into(),
        conflict: ConflictPolicy::Reject,
    };
    let prepared = service.prepare(&intent, &source).await.unwrap();
    let writing = service.begin(&intent, &source, &prepared).await.unwrap();
    service
        .write(
            &intent,
            &source,
            &writing,
            Box::pin(futures_util::stream::once(async { Ok(b"new".to_vec()) })),
        )
        .await
        .unwrap();
    let StreamStatus::Staged(staged) = service
        .probe(&intent, &source, &writing, 1024)
        .await
        .unwrap()
    else {
        panic!("expected verified staging");
    };
    let temporary =
        serde_json::from_slice::<serde_json::Value>(&staged.payload).unwrap()["temporary"]
            .as_str()
            .unwrap()
            .to_owned();
    // 返回已验证版本 HEAD 后马上改写，必须拒绝发布，不能先复制再发现摘要错误。
    *store.replace_after_head.lock().unwrap() = Some((
        temporary,
        Blob {
            bytes: b"bad".to_vec(),
            marker: "another-operation".into(),
            etag: "\"replacement-version\"".into(),
        },
    ));
    assert_eq!(
        service
            .publish(&intent, &source, &staged)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::SourceChanged
    );
    assert!(!store.blobs.lock().unwrap().contains_key("out"));
}
