//! 显式启动的真实 OSS 发布竞争验收；凭证仅由宿主环境提供。
#![cfg(feature = "oss")]
use std::sync::Arc;
use waybill::{
    error::ErrorKind,
    source::Source,
    transfer::ConflictPolicy,
    upload::{StreamStatus, UploadIntent},
};
use waybill_service_opendal::{ObjectStorage, ObjectStorageConfig, opendal::Operator};

#[test]
fn oss_upload_requires_the_multipart_marker_attribute() {
    use waybill::service::Service;
    let operator = Operator::via_iter(
        "oss",
        [
            ("bucket".into(), "test-bucket".into()),
            (
                "endpoint".into(),
                "https://oss-cn-shenzhen.aliyuncs.com".into(),
            ),
        ],
    )
    .unwrap();
    let mut config = ObjectStorageConfig::new("capability-test");
    config.conditional_writes = true;
    let supported = ObjectStorage::new(operator.clone(), config.clone()).unwrap();
    assert!(supported.info().capabilities.stream_upload);
    let operator = operator.layer(opendal::layers::CapabilityOverrideLayer::new(
        |mut capabilities| {
            capabilities.write_with_content_disposition = false;
            capabilities
        },
    ));
    let unsupported = ObjectStorage::new(operator, config).unwrap();
    assert!(!unsupported.info().capabilities.stream_upload);
    assert_eq!(
        unsupported.stream_upload_sink().err().unwrap().kind,
        ErrorKind::Unsupported
    );
}

#[tokio::test]
#[ignore = "requires dedicated OSS test bucket and explicit OSS_LIVE_* environment"]
async fn oss_publication_race_keeps_competing_object() {
    let bucket = std::env::var("OSS_LIVE_BUCKET").expect("test bucket required");
    assert!(bucket.starts_with("waybill-test-"));
    let run = std::env::var("OSS_LIVE_RUN").expect("unique test run required");
    assert!(!run.is_empty() && run.bytes().all(|b| b.is_ascii_hexdigit()));
    let operator = Operator::via_iter(
        "oss",
        [
            ("bucket".to_owned(), bucket.clone()),
            (
                "endpoint".to_owned(),
                "https://oss-cn-shenzhen.aliyuncs.com".into(),
            ),
            (
                "access_key_id".to_owned(),
                std::env::var("OSS_ACCESS_KEY_ID").expect("credential required"),
            ),
            (
                "access_key_secret".to_owned(),
                std::env::var("OSS_ACCESS_KEY_SECRET").expect("credential required"),
            ),
        ],
    )
    .unwrap_or_else(|_| panic!("invalid OSS test configuration"));
    // 锁定版本的 OSS 没有条件 copy，故本场景必须实际进入条件 writer 发布。
    assert!(!operator.info().capability().copy_with_if_not_exists);
    let mut config = ObjectStorageConfig::new(bucket);
    config.conditional_writes = true;
    let service = ObjectStorage::new(operator, config).expect("service configuration");
    let directory = tempfile::tempdir().expect("local test directory");
    let path = directory.path().join("race.bin");
    std::fs::write(&path, vec![42; 1024 * 1024]).expect("local test source");
    let source = Arc::new(
        waybill_service_fs::FileSource::open(path)
            .await
            .expect("stable source"),
    );
    let identity = source.identity().await.expect("source identity");
    let target = format!("scratch/race-{run}.bin");
    let intent = UploadIntent {
        operation: format!("race-{run}"),
        target: target.clone(),
        conflict: ConflictPolicy::Reject,
    };
    use waybill::upload::StreamUploadSink;
    let prepared = service.prepare(&intent, &identity).await.expect("prepare");
    let writing = service
        .begin(&intent, &identity, &prepared)
        .await
        .expect("begin");
    let body = Box::pin(futures_util::stream::try_unfold(
        (source, 0u64),
        |(source, offset)| async move {
            if offset == 1024 * 1024 {
                return Ok(None);
            }
            let data = source.read_range(offset, 256 * 1024).await?;
            Ok(Some((data, (source, offset + 256 * 1024))))
        },
    ));
    service
        .write(&intent, &identity, &writing, body)
        .await
        .expect("temporary upload");
    let StreamStatus::Staged(staged) = service
        .probe(&intent, &identity, &writing, 1024 * 1024)
        .await
        .expect("verify staging")
    else {
        panic!("expected verified staging");
    };
    service
        .operator()
        .write_with(&target, "competing object")
        .if_not_exists(true)
        .await
        .unwrap_or_else(|_| panic!("cannot create competing test object"));
    assert_eq!(
        service
            .publish(&intent, &identity, &staged)
            .await
            .expect_err("must reject competing destination")
            .kind,
        ErrorKind::Conflict
    );
    let bytes = service
        .operator()
        .read(&target)
        .await
        .unwrap_or_else(|_| panic!("cannot verify competing object"));
    assert_eq!(bytes.to_vec(), b"competing object");
    println!(
        "PASS: real OSS conditional writer publication rejects race and preserves competing bytes"
    );
}

#[derive(Default)]
struct MultipartObject {
    disposition: String,
    bytes: Vec<u8>,
    completed: bool,
}
#[derive(Clone, Default)]
struct MultipartServer(Arc<std::sync::Mutex<MultipartObject>>);
impl wiremock::Respond for MultipartServer {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        use wiremock::ResponseTemplate as R;
        let mut object = self.0.lock().unwrap();
        let query: std::collections::BTreeMap<_, _> = request.url.query_pairs().collect();
        match request.method.as_str() {
            "POST" if query.contains_key("uploads") => {
                assert_eq!(
                    request.headers.get("x-oss-forbid-overwrite").unwrap(),
                    "true"
                );
                // 模拟锁定 OpenDAL 版本的 OSS 初始化：没有用户元数据，必须携带扩展参数。
                assert!(!request.headers.contains_key("x-oss-meta-waybill-delivery"));
                object.disposition = request
                    .headers
                    .get("content-disposition")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .into();
                assert!(
                    object
                        .disposition
                        .starts_with("inline; waybill-delivery=\"")
                );
                R::new(200).set_body_string("<InitiateMultipartUploadResult><UploadId>test-upload</UploadId></InitiateMultipartUploadResult>")
            }
            "PUT" if query.contains_key("partNumber") => {
                object.bytes.extend_from_slice(&request.body);
                R::new(200).insert_header("etag", "\"part-etag\"")
            }
            "POST" if query.contains_key("uploadId") => {
                assert_eq!(
                    request.headers.get("x-oss-forbid-overwrite").unwrap(),
                    "true"
                );
                object.completed = true;
                R::new(200).set_body_string("<CompleteMultipartUploadResult><ETag>\"object-etag\"</ETag></CompleteMultipartUploadResult>")
            }
            "HEAD" if object.completed && request.url.path().contains(".waybill-") => R::new(200)
                .insert_header("content-length", object.bytes.len().to_string())
                .insert_header("etag", "\"object-etag\"")
                .insert_header("content-disposition", &object.disposition),
            "HEAD" => R::new(404),
            "GET" if object.completed => {
                assert_eq!(request.headers.get("if-match").unwrap(), "\"object-etag\"");
                let range = request
                    .headers
                    .get("range")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_prefix("bytes=")
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start: usize = start.parse().unwrap();
                let end: usize = end.parse().unwrap();
                R::new(206)
                    .insert_header("etag", "\"object-etag\"")
                    .insert_header(
                        "content-range",
                        format!("bytes {start}-{end}/{}", object.bytes.len()),
                    )
                    .set_body_bytes(object.bytes[start..=end].to_vec())
            }
            _ => R::new(500),
        }
    }
}
#[tokio::test]
async fn oss_multipart_staging_retains_ownership_without_user_metadata() {
    let server = wiremock::MockServer::start().await;
    let response = MultipartServer::default();
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(response.clone())
        .mount(&server)
        .await;
    let operator = Operator::via_iter(
        "oss",
        [
            ("bucket".to_owned(), "test".to_owned()),
            ("endpoint".to_owned(), server.uri()),
            ("addressing_style".to_owned(), "path".into()),
            ("access_key_id".to_owned(), "local-test".into()),
            ("access_key_secret".to_owned(), "local-secret".into()),
        ],
    )
    .unwrap();
    let mut config = ObjectStorageConfig::new("local-oss-test");
    config.conditional_writes = true;
    let service = ObjectStorage::new(operator, config).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.bin");
    std::fs::write(&path, vec![42; 256 * 1024]).unwrap();
    let source = Arc::new(waybill_service_fs::FileSource::open(path).await.unwrap());
    let identity = source.identity().await.unwrap();
    let intent = UploadIntent {
        operation: "oss-multipart-marker".into(),
        target: "out".into(),
        conflict: ConflictPolicy::Reject,
    };
    use waybill::upload::StreamUploadSink;
    let prepared = service.prepare(&intent, &identity).await.unwrap();
    let writing = service.begin(&intent, &identity, &prepared).await.unwrap();
    let body = Box::pin(futures_util::stream::once(async move {
        source.read_range(0, 256 * 1024).await
    }));
    service
        .write(&intent, &identity, &writing, body)
        .await
        .unwrap();
    assert!(matches!(
        service
            .probe(&intent, &identity, &writing, 1024 * 1024)
            .await
            .unwrap(),
        StreamStatus::Staged(_)
    ));
    assert_eq!(response.0.lock().unwrap().bytes.len(), 256 * 1024);
}
