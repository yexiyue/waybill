use super::*;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_bytes, header, method, path},
};
fn service(server: &MockServer) -> Webdav {
    crate::tests::service(&format!("{}/dav/", server.uri()))
}
fn input() -> (UploadIntent, SourceIdentity) {
    (
        UploadIntent {
            operation: "op".into(),
            target: "a.txt".into(),
            conflict: ConflictPolicy::Reject,
        },
        SourceIdentity {
            reference: "source".into(),
            revision: "v1".into(),
            size: 4,
            blake3: blake3::hash(b"abcd").to_hex().to_string(),
        },
    )
}
fn xml(reference: &str, marker: Option<&str>) -> String {
    let marker = marker
        .map(|value| format!("<w:delivery>{value}</w:delivery>"))
        .unwrap_or_default();
    format!(
        r#"<d:multistatus xmlns:d="DAV:" xmlns:w="urn:waybill"><d:response><d:href>/dav/{reference}</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>4</d:getcontentlength><d:getetag>"v1"</d:getetag>{marker}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
    )
}
#[tokio::test]
async fn temporary_names_are_random_and_state_is_bound_to_source_and_intent() {
    let server = MockServer::start().await;
    let dav = service(&server);
    let (intent, source) = input();
    let one = dav.prepare(&intent, &source).await.unwrap();
    let two = dav.prepare(&intent, &source).await.unwrap();
    assert_ne!(one, two);
    let mut changed = source.clone();
    changed.revision = "v2".into();
    assert_eq!(
        dav.begin(&intent, &changed, &one).await.unwrap_err().kind,
        ErrorKind::Checkpoint
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn invalid_or_legacy_upload_phases_are_rejected_without_remote_mutation() {
    let server = MockServer::start().await;
    let dav = service(&server);
    let (intent, source) = input();
    let saved = dav.prepare(&intent, &source).await.unwrap();
    let mut old = saved.clone();
    old.version = 1;
    assert_eq!(
        dav.begin(&intent, &source, &old).await.unwrap_err().kind,
        ErrorKind::Checkpoint
    );
    for phase in [
        serde_json::json!({"phase":"staged"}),
        serde_json::json!({"phase":"staged","etag":"W/\"weak\""}),
    ] {
        let mut payload: serde_json::Value = serde_json::from_slice(&saved.payload).unwrap();
        for (key, value) in phase.as_object().unwrap() {
            payload[key] = value.clone();
        }
        let invalid = DriverState {
            version: saved.version,
            payload: serde_json::to_vec(&payload).unwrap(),
        };
        assert_eq!(
            dav.begin(&intent, &source, &invalid)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Checkpoint
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn put_uses_create_condition_then_version_condition_for_explicit_restart() {
    let server = MockServer::start().await;
    let dav = service(&server);
    let (intent, source) = input();
    let saved = dav.prepare(&intent, &source).await.unwrap();
    let saved = dav.begin(&intent, &source, &saved).await.unwrap();
    let mut state = dav.upload_state(&intent, &source, &saved).unwrap();
    Mock::given(method("PUT"))
        .and(path(format!("/dav/{}", state.temporary)))
        .and(header("if-none-match", "*"))
        .and(header("content-length", "4"))
        .and(body_bytes(b"abcd".to_vec()))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    let body = || Box::pin(futures_util::stream::iter([Ok(b"abcd".to_vec())])) as UploadBody;
    dav.write(&intent, &source, &saved, body()).await.unwrap();
    state.phase = Phase::Writing {
        etag: Some("\"v1\"".into()),
    };
    Mock::given(method("PUT"))
        .and(path(format!("/dav/{}", state.temporary)))
        .and(header("if-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        dav.write(&intent, &source, &state.encode().unwrap(), body())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
}
#[tokio::test]
async fn same_length_final_object_without_operation_marker_is_a_conflict() {
    let server = MockServer::start().await;
    let dav = service(&server);
    let (intent, source) = input();
    let saved = dav.prepare(&intent, &source).await.unwrap();
    Mock::given(method("PROPFIND"))
        .and(path("/dav/a.txt"))
        .respond_with(ResponseTemplate::new(207).set_body_string(xml("a.txt", None)))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        dav.probe(&intent, &source, &saved, 2)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
#[tokio::test]
async fn move_is_version_conditional_and_never_overwrites_destination() {
    let server = MockServer::start().await;
    let dav = service(&server);
    let (intent, source) = input();
    let saved = dav.prepare(&intent, &source).await.unwrap();
    let mut state = dav.upload_state(&intent, &source, &saved).unwrap();
    state.phase = Phase::Staged {
        etag: "\"v1\"".into(),
    };
    Mock::given(method("PROPFIND"))
        .and(path(format!("/dav/{}", state.temporary)))
        .respond_with(
            ResponseTemplate::new(207)
                .set_body_string(xml(&state.temporary, Some(&state.marker()))),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("MOVE"))
        .and(path(format!("/dav/{}", state.temporary)))
        .and(header("overwrite", "F"))
        .and(header("if-match", "\"v1\""))
        .and(header("destination", format!("{}/dav/a.txt", server.uri())))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        dav.publish(&intent, &source, &state.encode().unwrap())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
}
#[tokio::test]
async fn missing_dead_property_support_never_reaches_move() {
    let server = MockServer::start().await;
    let dav = service(&server);
    let (intent, source) = input();
    let saved = dav.prepare(&intent, &source).await.unwrap();
    let mut state = dav.upload_state(&intent, &source, &saved).unwrap();
    state.phase = Phase::Staged {
        etag: "\"v1\"".into(),
    };
    Mock::given(method("PROPFIND"))
        .and(path(format!("/dav/{}", state.temporary)))
        .respond_with(ResponseTemplate::new(207).set_body_string(xml(&state.temporary, None)))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PROPPATCH"))
        .and(header("if-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(207))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        dav.publish(&intent, &source, &state.encode().unwrap())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method.as_str() != "MOVE")
    );
}

#[derive(Default)]
struct RemoteState {
    temporary: Option<String>,
    marker: Option<String>,
    published: bool,
    puts: usize,
    patches: usize,
    moves: usize,
}
#[derive(Clone)]
struct FaultServer {
    state: Arc<std::sync::Mutex<RemoteState>>,
    lose_patch: bool,
}
impl wiremock::Respond for FaultServer {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let reference = request.url.path().strip_prefix("/dav/").unwrap();
        let mut state = self.state.lock().unwrap();
        match request.method.as_str() {
            "PUT" => {
                assert_eq!(request.headers.get("if-none-match").unwrap(), "*");
                assert_eq!(request.body, b"abcd");
                state.temporary = Some(reference.into());
                state.puts += 1;
                ResponseTemplate::new(503) // 对象已完整落地，但调用方没有收到成功。
            }
            "PROPPATCH" => {
                assert_eq!(request.headers.get("if-match").unwrap(), "\"v1\"");
                let body = std::str::from_utf8(&request.body).unwrap();
                state.marker = Some(
                    body.split("<w:delivery>")
                        .nth(1)
                        .unwrap()
                        .split("</w:delivery>")
                        .next()
                        .unwrap()
                        .into(),
                );
                state.patches += 1;
                ResponseTemplate::new(if self.lose_patch { 503 } else { 207 })
            }
            "MOVE" => {
                assert_eq!(request.headers.get("overwrite").unwrap(), "F");
                assert_eq!(request.headers.get("if-match").unwrap(), "\"v1\"");
                assert!(state.marker.is_some());
                state.published = true;
                state.moves += 1;
                ResponseTemplate::new(503) // MOVE 已生效；只能用最终对象和标记对账。
            }
            verb => {
                let exists = if reference == "a.txt" {
                    state.published
                } else {
                    !state.published && state.temporary.as_deref() == Some(reference)
                };
                if !exists {
                    return ResponseTemplate::new(404);
                }
                match verb {
                    "PROPFIND" => ResponseTemplate::new(207)
                        .set_body_string(xml(reference, state.marker.as_deref())),
                    "HEAD" => ResponseTemplate::new(200)
                        .insert_header("etag", "\"v1\"")
                        .set_body_bytes(b"abcd".to_vec()),
                    "GET" => {
                        assert_eq!(request.headers.get("if-match").unwrap(), "\"v1\"");
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
                        ResponseTemplate::new(206)
                            .insert_header("etag", "\"v1\"")
                            .insert_header("content-range", format!("bytes {start}-{end}/4"))
                            .set_body_bytes(b"abcd"[start..=end].to_vec())
                    }
                    _ => ResponseTemplate::new(405),
                }
            }
        }
    }
}
#[tokio::test]
async fn applied_put_and_move_with_lost_responses_have_one_delivery() {
    response_loss(false).await;
}
#[tokio::test]
async fn applied_proppatch_with_lost_response_only_retries_publish() {
    response_loss(true).await;
}
async fn response_loss(lose_patch: bool) {
    use waybill::{TransferEngine, UploadOptions, budget::ResourceBudget};
    use waybill_service_fs::{FileCheckpointStore, FileSource};
    let server = MockServer::start().await;
    let fixture = FaultServer {
        state: Arc::new(std::sync::Mutex::new(RemoteState::default())),
        lose_patch,
    };
    Mock::given(wiremock::matchers::any())
        .respond_with(fixture.clone())
        .mount(&server)
        .await;
    let dav = service(&server);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    std::fs::write(&path, b"abcd").unwrap();
    let source = Arc::new(FileSource::open(path).await.unwrap());
    let engine = TransferEngine::new(FileCheckpointStore::new(dir.path().join("records")))
        .with_budget(Arc::new(ResourceBudget::new(2, 1).unwrap()));
    let options = || UploadOptions::new("lost-response", "a.txt");
    if lose_patch {
        assert_eq!(
            engine
                .upload_stream(source.clone(), &dav, options())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Retryable
        );
    }
    let receipt = engine
        .upload_stream(source.clone(), &dav, options())
        .await
        .unwrap();
    assert_eq!(
        engine.upload_stream(source, &dav, options()).await.unwrap(),
        receipt
    );
    let state = fixture.state.lock().unwrap();
    assert_eq!((state.puts, state.patches, state.moves), (1, 1, 1));
    assert!(state.published);
}
