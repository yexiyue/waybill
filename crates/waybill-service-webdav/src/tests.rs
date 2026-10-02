//! HTTP 替身验证边界；Docker 测试单独显式运行，不把替身结果当服务端验收。
use super::*;
use crate::credential::{Authentication, Credentials};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

pub(super) fn service(endpoint: &str) -> Webdav {
    Webdav::new(
        WebdavConfig::new(endpoint, "anonymous"),
        Arc::new(Credentials {
            authentication: Authentication::Anonymous,
            username: String::new(),
            password: String::new(),
        }),
    )
    .unwrap()
}
fn xml(href: &str, directory: bool, size: u64, etag: &str) -> String {
    format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>{href}</D:href><D:propstat><D:prop><D:resourcetype>{}</D:resourcetype><D:getcontentlength>{size}</D:getcontentlength><D:getetag>{etag}</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#,
        if directory { "<D:collection/>" } else { "" }
    )
}
async fn media_server(etag: &str, probe: ResponseTemplate) -> (MockServer, Webdav) {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/a.bin"))
        .and(header("depth", "0"))
        .respond_with(ResponseTemplate::new(207).set_body_string(xml("/dav/a.bin", false, 4, etag)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/dav/a.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", etag)
                .set_body_bytes(b"abcd".to_vec()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/a.bin"))
        .and(header("range", "bytes=0-0"))
        .and(header("if-match", etag))
        .respond_with(probe)
        .mount(&server)
        .await;
    let dav = service(&format!("{}/dav/", server.uri()));
    (server, dav)
}
fn range_response(start: u64, end: u64, body: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(206)
        .insert_header("content-range", format!("bytes {start}-{end}/4"))
        .insert_header("etag", "\"v1\"")
        .set_body_bytes(body)
}
#[test]
fn paths_preserve_literal_names_and_confine_hrefs() {
    let paths = crate::path::Paths::new("https://example.test/dav/root/").unwrap();
    let url = paths.url("中文 空格/%2f?#.txt").unwrap();
    assert_eq!(
        paths.reference(url.as_str(), false).unwrap(),
        "中文 空格/%2f?#.txt"
    );
    for href in [
        "https://other.test/dav/root/a",
        "/other/a",
        "/dav/root/%2fetc",
        "/dav/root/%2e%2e/a",
        "/dav/root/a?token=x",
        "/dav/root/a//b",
        "/dav/root//a",
    ] {
        assert!(paths.reference(href, false).is_err(), "{href}");
    }
}
#[test]
fn instance_is_bound_to_endpoint_principal_and_not_password() {
    let one = service("https://example.test/dav/");
    let slash = service("https://example.test/dav");
    assert_eq!(one.info().identity, slash.info().identity);
    let authenticated = |username: &str, password: &str| {
        Webdav::new(
            WebdavConfig::new("https://example.test/dav/", username),
            Arc::new(Credentials {
                authentication: Authentication::Basic,
                username: username.into(),
                password: password.into(),
            }),
        )
        .unwrap()
        .info()
        .identity
    };
    assert_eq!(authenticated("alice", "old"), authenticated("alice", "new"));
    assert_ne!(authenticated("alice", "old"), authenticated("bob", "old"));
    assert_ne!(
        one.info().identity,
        service("https://example.test/other/").info().identity
    );
    for endpoint in [
        "ftp://example.test/",
        "https://user:secret@example.test/",
        "https://example.test/?q=x",
    ] {
        assert!(
            Webdav::new(
                WebdavConfig::new(endpoint, "user"),
                Arc::new(Credentials {
                    authentication: Authentication::Basic,
                    username: "user".into(),
                    password: "secret".into()
                })
            )
            .is_err()
        );
    }
}
#[tokio::test]
async fn propfind_merges_successful_property_groups_and_rejects_escape() {
    let server = MockServer::start().await;
    let body = r#"<multistatus xmlns="DAV:"><response><href>/dav/a</href><propstat><prop><resourcetype/></prop><status>HTTP/1.1 200 OK</status></propstat><propstat><prop><getcontentlength>4</getcontentlength></prop><status>HTTP/1.1 200 OK</status></propstat><propstat><prop><getetag>denied</getetag></prop><status>HTTP/1.1 403 Forbidden</status></propstat></response></multistatus>"#;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_string(body))
        .mount(&server)
        .await;
    let dav = service(&format!("{}/dav/", server.uri()));
    let entry = dav.resolve("a").await.unwrap();
    assert_eq!(entry.size, Some(4));
    assert_eq!(entry.reference, "a");
    server.reset().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_string(xml(
            "https://other.test/a",
            false,
            4,
            "v1",
        )))
        .mount(&server)
        .await;
    assert_eq!(
        dav.resolve("a").await.unwrap_err().kind,
        ErrorKind::Protocol
    );
}
#[tokio::test]
async fn listing_requires_parent_and_direct_unique_children() {
    let server = MockServer::start().await;
    let dav = service(&format!("{}/dav/", server.uri()));
    let parent = xml("/dav/", true, 0, "");
    let child = xml("/dav/a", false, 4, "v1");
    let body = parent.replace(
        "</D:multistatus>",
        &child.replace("<D:multistatus xmlns:D=\"DAV:\">", ""),
    );
    Mock::given(method("PROPFIND"))
        .and(header("depth", "1"))
        .respond_with(ResponseTemplate::new(207).set_body_string(body))
        .mount(&server)
        .await;
    let listed = dav.list("/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].reference, "a");
    server.reset().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_string(child))
        .mount(&server)
        .await;
    assert_eq!(dav.list("/").await.unwrap_err().kind, ErrorKind::Protocol);
}
#[tokio::test]
async fn child_only_listing_confirms_parent_with_depth_zero() {
    let server = MockServer::start().await;
    let dav = service(&format!("{}/dav/", server.uri()));
    Mock::given(method("PROPFIND"))
        .and(header("depth", "1"))
        .respond_with(ResponseTemplate::new(207).set_body_string(xml("/dav/a", false, 4, "v1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(header("depth", "0"))
        .respond_with(ResponseTemplate::new(207).set_body_string(xml("/dav/", true, 0, "")))
        .expect(1)
        .mount(&server)
        .await;
    let listed = dav.list("/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].reference, "a");
}
#[tokio::test]
async fn listing_limit_applies_even_when_parent_is_omitted() {
    for include_parent in [true, false] {
        let server = MockServer::start().await;
        let mut body = String::from("<D:multistatus xmlns:D=\"DAV:\">");
        let entries = (0..1001).map(|index| xml(&format!("/dav/f{index}"), false, 1, "v1"));
        for entry in include_parent
            .then(|| xml("/dav/", true, 0, ""))
            .into_iter()
            .chain(entries)
        {
            body.push_str(
                &entry
                    .replace("<D:multistatus xmlns:D=\"DAV:\">", "")
                    .replace("</D:multistatus>", ""),
            );
        }
        body.push_str("</D:multistatus>");
        Mock::given(method("PROPFIND"))
            .and(header("depth", "1"))
            .respond_with(ResponseTemplate::new(207).set_body_string(body))
            .mount(&server)
            .await;
        assert_eq!(
            service(&format!("{}/dav/", server.uri()))
                .list("/")
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Protocol
        );
    }
}
#[tokio::test]
async fn range_ignoring_server_and_weak_etag_are_rejected_before_download() {
    let (_server, dav) = media_server(
        "\"v1\"",
        ResponseTemplate::new(200).set_body_bytes(b"abcd".to_vec()),
    )
    .await;
    assert!(matches!(dav.download_source("a.bin").await,Err(e) if e.kind==ErrorKind::Unsupported));
    let (_server, dav) = media_server("W/\"v1\"", ResponseTemplate::new(200)).await;
    assert!(matches!(dav.download_source("a.bin").await,Err(e) if e.kind==ErrorKind::Unsupported));
}
#[tokio::test]
async fn range_is_conditional_and_source_change_is_never_downloaded() {
    let (server, dav) = media_server("\"v1\"", range_response(0, 0, b"a")).await;
    Mock::given(method("GET"))
        .and(header("range", "bytes=1-2"))
        .and(header("if-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&server)
        .await;
    let source = dav.download_source("a.bin").await.unwrap();
    assert_eq!(
        source.read_range(1, 2).await.unwrap_err().kind,
        ErrorKind::SourceChanged
    );
}
#[tokio::test]
async fn range_metadata_and_length_are_checked_before_accepting_bytes() {
    for response in [
        range_response(1, 1, b"a"),
        range_response(0, 0, b"ab"),
        range_response(0, 0, b"a").insert_header("etag", "\"v2\""),
    ] {
        let (_server, dav) = media_server("\"v1\"", response).await;
        assert!(matches!(dav.download_source("a.bin").await,Err(e) if e.kind==ErrorKind::Protocol));
    }
}
#[tokio::test]
async fn redirects_do_not_forward_credentials_or_follow_external_urls() {
    let original = MockServer::start().await;
    let external = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/leak", external.uri())),
        )
        .expect(1)
        .mount(&original)
        .await;
    let dav = service(&original.uri());
    assert_eq!(
        dav.resolve("/").await.unwrap_err().kind,
        ErrorKind::Protocol
    );
    assert!(external.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn oversized_and_non_dav_xml_are_rejected() {
    for body in [
        vec![b'x'; 1024 * 1024 + 1],
        b"<multistatus xmlns=\"wrong\"/>".to_vec(),
        b"<!DOCTYPE x><multistatus xmlns=\"DAV:\"/>".to_vec(),
        xml("/", true, 0, "\"v1\"")
            .replace("<D:resourcetype>", "<x:resourcetype xmlns:x=\"not-DAV\">")
            .replace("</D:resourcetype>", "</x:resourcetype>")
            .into_bytes(),
        xml("/", true, 0, "\"v1\"")
            .replace(
                "</D:prop>",
                "<w:delivery xmlns:w=\"not-waybill\">forged</w:delivery></D:prop>",
            )
            .into_bytes(),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("PROPFIND"))
            .respond_with(ResponseTemplate::new(207).set_body_bytes(body))
            .mount(&server)
            .await;
        assert_eq!(
            service(&server.uri()).resolve("/").await.unwrap_err().kind,
            ErrorKind::Protocol
        );
    }
}

#[tokio::test]
#[ignore = "requires tests/webdav/compose.yml"]
async fn docker_basic_and_digest_download_through_public_contract() {
    use waybill::{
        budget::ResourceBudget,
        checkpoint::{CheckpointStore, Flow},
        download::DownloadOptions,
        transfer::TransferEngine,
    };
    use waybill_service_fs::{FileCheckpointStore, FsService};
    for (folder, authentication, expected) in [
        ("basic", Authentication::Basic, b"basic seed\n".as_slice()),
        (
            "digest",
            Authentication::Digest,
            b"digest seed\n".as_slice(),
        ),
    ] {
        let dav = docker_service(folder, authentication);
        let root = dav.resolve("/").await.unwrap();
        assert!(root.is_directory());
        assert!(
            dav.list(&root.reference)
                .await
                .unwrap()
                .iter()
                .any(|file| file.name == "seed.txt")
        );
        let file = dav.resolve("seed.txt").await.unwrap();
        let source = dav.download_source(&file.reference).await.unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("received.txt");
        let store = FileCheckpointStore::new(tmp.path().join("records"));
        let engine = TransferEngine::new(store.clone())
            .with_budget(Arc::new(ResourceBudget::new(4, 2).unwrap()));
        let local = FsService::new("webdav-test-local").unwrap();
        let sink = local.download_target().unwrap();
        let options = || DownloadOptions::new("download-test", target.to_str().unwrap());
        let receipt = engine
            .download(source.as_ref(), sink.as_ref(), options())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), expected);
        assert_eq!(
            engine
                .download(source.as_ref(), sink.as_ref(), options())
                .await
                .unwrap(),
            receipt
        );
        let lease = store.acquire("download-test").await.unwrap();
        let checkpoint = lease.load().await.unwrap().unwrap();
        assert!(matches!(checkpoint.flow,Flow::Download(flow) if flow.receipt==Some(receipt)));
    }
}

#[tokio::test]
#[ignore = "requires tests/webdav/compose.yml"]
async fn docker_upload_download_empty_files_conflicts_and_named_roots() {
    use waybill::{
        DownloadOptions, TransferEngine, UploadOptions,
        budget::ResourceBudget,
        content::{DigestAlgorithm, Verification},
        source::Source,
        transfer::ConflictPolicy,
    };
    use waybill_service_fs::{FileCheckpointStore, FileSource, FsService};
    for (folder, authentication) in [
        ("basic", Authentication::Basic),
        ("digest", Authentication::Digest),
    ] {
        let dav = docker_service(folder, authentication);
        let temporary = tempfile::tempdir().unwrap();
        let root = format!(
            "contract-{}/目录/",
            temporary
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .trim_start_matches('.')
        );
        let store = FileCheckpointStore::new(temporary.path().join("records"));
        let engine = TransferEngine::new(store.clone())
            .with_budget(Arc::new(ResourceBudget::new(4096, 2).unwrap()));
        for size in [0, 31, 65536] {
            let bytes: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
            let local_path = temporary.path().join(format!("source-{size}"));
            std::fs::write(&local_path, &bytes).unwrap();
            let source = Arc::new(FileSource::open(&local_path).await.unwrap());
            let identity = source.identity().await.unwrap();
            let target = format!("{root}100% 文件-{size}.bin");
            let operation = format!("upload-{size}");
            let options = || UploadOptions::new(&operation, &target);
            let receipt = engine
                .upload_stream(source.clone(), &dav, options())
                .await
                .unwrap();
            assert_eq!(
                receipt.verified,
                Verification::Digest {
                    algorithm: DigestAlgorithm::Blake3,
                    value: identity.blake3.clone()
                }
            );
            assert_eq!(
                engine
                    .upload_stream(source.clone(), &dav, options())
                    .await
                    .unwrap(),
                receipt
            );
            let remote = dav.download_source(&receipt.object).await.unwrap();
            let sink = FsService::new("webdav-roundtrip")
                .unwrap()
                .download_target()
                .unwrap();
            let received = temporary.path().join(format!("received-{size}"));
            let download_options =
                || DownloadOptions::new(format!("download-{size}"), received.to_str().unwrap());
            if size == 65536 {
                use waybill::{checkpoint::CheckpointStore, transfer::StopToken};
                let stop = StopToken::default();
                let progress = |p: waybill::download::DownloadProgress| {
                    if p.persisted > 0 {
                        stop.stop();
                    }
                };
                let mut first = download_options();
                first.stop = stop.clone();
                first.progress = Some(&progress);
                assert_eq!(
                    engine
                        .download(remote.as_ref(), sink.as_ref(), first)
                        .await
                        .unwrap_err()
                        .kind,
                    ErrorKind::Paused
                );
                let persisted = {
                    let lease = store.acquire(&format!("download-{size}")).await.unwrap();
                    let saved = lease.load().await.unwrap().unwrap();
                    saved.download().unwrap().persisted_bytes().unwrap()
                };
                assert!(persisted > 0 && persisted < size as u64);
                assert!(!received.exists());
                let resumed = TransferEngine::new(store.clone())
                    .with_budget(Arc::new(ResourceBudget::new(4096, 2).unwrap()));
                let fresh = RecordedDownload {
                    inner: dav.download_source(&receipt.object).await.unwrap(),
                    offsets: std::sync::Mutex::new(Vec::new()),
                };
                resumed
                    .download(&fresh, sink.as_ref(), download_options())
                    .await
                    .unwrap();
                let offsets = fresh.offsets.lock().unwrap();
                assert!(!offsets.is_empty() && offsets.iter().all(|offset| *offset >= persisted));
            } else {
                engine
                    .download(remote.as_ref(), sink.as_ref(), download_options())
                    .await
                    .unwrap();
            }
            assert_eq!(std::fs::read(received).unwrap(), bytes);
            let conflicting = UploadOptions::new(format!("conflict-{size}"), &target);
            assert_eq!(
                engine
                    .upload_stream(source.clone(), &dav, conflicting)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Conflict
            );
            let mut suffixed = UploadOptions::new(format!("suffix-{size}"), &target);
            suffixed.intent.conflict = ConflictPolicy::OperationSuffix;
            let alternative = engine.upload_stream(source, &dav, suffixed).await.unwrap();
            assert_ne!(alternative.object, receipt.object);
        }
        let rooted = dav.at_root(&root).unwrap();
        let listing = rooted.list("/").await.unwrap();
        assert_eq!(listing.len(), 6);
        assert!(
            listing
                .iter()
                .all(|file| !file.reference.contains('/') && !file.name.starts_with(".waybill-"))
        );
        assert_ne!(rooted.info().identity, dav.info().identity);
    }
}

fn docker_service(folder: &str, authentication: Authentication) -> Webdav {
    let port = std::env::var("WAYBILL_WEBDAV_TEST_PORT").unwrap_or_else(|_| "18765".into());
    let port: u16 = port.parse().expect("test server port must be u16");
    Webdav::new(
        WebdavConfig::new(format!("http://127.0.0.1:{port}/{folder}/"), "tester"),
        Arc::new(Credentials {
            authentication,
            username: "tester".into(),
            password: "fixture-password".into(),
        }),
    )
    .unwrap()
}

struct RecordedDownload {
    inner: Arc<dyn waybill::download::DownloadSource>,
    offsets: std::sync::Mutex<Vec<u64>>,
}
impl waybill::download::DownloadSource for RecordedDownload {
    fn max_read_size(&self) -> usize {
        self.inner.max_read_size()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn identity(&self) -> BoxFuture<'_, waybill::download::RemoteIdentity> {
        self.inner.identity()
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        self.offsets.lock().unwrap().push(offset);
        self.inner.read_range(offset, length)
    }
}

#[tokio::test]
#[ignore = "requires tests/webdav/compose.yml"]
async fn docker_interrupted_put_requires_explicit_whole_file_restart() {
    use waybill::{
        BoxFuture, TransferEngine, UploadOptions,
        budget::ResourceBudget,
        source::{Source, SourceIdentity},
        transfer::StopToken,
    };
    use waybill_service_fs::{FileCheckpointStore, FileSource};
    struct PausingSource {
        inner: Arc<dyn Source>,
        stop: StopToken,
    }
    impl Source for PausingSource {
        fn max_read_size(&self) -> usize {
            self.inner.max_read_size()
        }
        fn identity(&self) -> BoxFuture<'_, SourceIdentity> {
            self.inner.identity()
        }
        fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
            Box::pin(async move {
                let bytes = self.inner.read_range(offset, length).await?;
                if offset >= 4096 {
                    self.stop.stop();
                }
                Ok(bytes)
            })
        }
    }
    for (folder, authentication) in [
        ("basic", Authentication::Basic),
        ("digest", Authentication::Digest),
    ] {
        let dav = docker_service(folder, authentication);
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("source");
        std::fs::write(&file, vec![37u8; 65536]).unwrap();
        let source = Arc::new(FileSource::open(file).await.unwrap());
        let target = format!(
            "interrupt-{}/file.bin",
            temporary
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .trim_start_matches('.')
        );
        let engine =
            TransferEngine::new(FileCheckpointStore::new(temporary.path().join("records")))
                .with_budget(Arc::new(ResourceBudget::new(4096, 1).unwrap()));
        let stop = StopToken::default();
        let paused = Arc::new(PausingSource {
            inner: source.clone(),
            stop: stop.clone(),
        });
        let options = || UploadOptions::new("interrupted", &target);
        let mut first = options();
        first.stop = stop;
        assert_eq!(
            engine
                .upload_stream(paused, &dav, first)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Paused
        );
        assert_eq!(
            engine
                .upload_stream(source.clone(), &dav, options())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::SessionExpired
        );
        assert_eq!(
            dav.resolve(&target).await.unwrap_err().kind,
            ErrorKind::NotFound
        );
        let mut restart = options();
        restart.policy.allow_restart = true;
        let receipt = engine
            .upload_stream(source.clone(), &dav, restart)
            .await
            .unwrap();
        assert_eq!(receipt.size, 65536);
        assert_eq!(
            engine.upload_stream(source, &dav, options()).await.unwrap(),
            receipt
        );
    }
}
