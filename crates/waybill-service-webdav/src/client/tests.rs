//! 真实本机 HTTP 验证总时限覆盖请求与正文，并区分普通请求和整文件 PUT。
use super::*;
use crate::{Webdav, credential::Credentials};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn client(endpoint: &str, request: Duration, upload: Duration) -> DavClient {
    let mut config = WebdavConfig::new(endpoint, "anonymous");
    config.request_timeout = request;
    config.upload_timeout = upload;
    DavClient::new(
        Paths::new(endpoint).unwrap(),
        config,
        Arc::new(Credentials {
            authentication: Authentication::Anonymous,
            username: String::new(),
            password: String::new(),
        }),
    )
    .unwrap()
}

#[test]
fn timeout_configuration_rejects_zero_and_excessive_deadlines() {
    for timeout in [Duration::ZERO, Duration::from_secs(24 * 3600 + 1)] {
        for upload in [false, true] {
            let mut config = WebdavConfig::new("https://example.test/", "anonymous");
            if upload {
                config.upload_timeout = timeout;
            } else {
                config.request_timeout = timeout;
            }
            let error = Webdav::new(
                config,
                Arc::new(Credentials {
                    authentication: Authentication::Anonymous,
                    username: String::new(),
                    password: String::new(),
                }),
            )
            .err()
            .unwrap();
            assert_eq!(error.kind, ErrorKind::InvalidInput);
        }
    }
}

#[tokio::test]
async fn ordinary_request_times_out_before_response_headers() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    let api = client(
        &server.uri(),
        Duration::from_millis(60),
        Duration::from_secs(1),
    );
    let error = api
        .request(Method::HEAD, "/")
        .await
        .unwrap()
        .send()
        .await
        .unwrap_err();
    assert!(error.is_timeout());
}

#[tokio::test]
async fn total_deadline_does_not_reset_when_response_body_makes_progress() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let received = socket.read(&mut request).await.unwrap();
        assert!(received > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\na")
            .await
            .unwrap();
        for byte in *b"bcd" {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if socket.write_all(&[byte]).await.is_err() {
                break;
            }
        }
    });
    let api = client(
        &endpoint,
        Duration::from_millis(250),
        Duration::from_secs(1),
    );
    let response = api
        .request(Method::GET, "/")
        .await
        .unwrap()
        .send()
        .await
        .unwrap();
    assert!(response.bytes().await.unwrap_err().is_timeout());
    server.await.unwrap();
}

#[tokio::test]
async fn put_uses_its_own_deadline_and_remains_bounded() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    let body = || {
        reqwest::Body::wrap_stream(futures_util::stream::unfold(0, |offset| async move {
            if offset == 2 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            Some((Ok::<_, std::io::Error>(vec![b'a']), offset + 1))
        }))
    };
    let api = client(
        &server.uri(),
        Duration::from_millis(60),
        Duration::from_secs(1),
    );
    let response = api
        .request(Method::PUT, "/")
        .await
        .unwrap()
        .header("Content-Length", 2)
        .body(body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let api = client(
        &server.uri(),
        Duration::from_secs(1),
        Duration::from_millis(60),
    );
    let error = api
        .request(Method::PUT, "/")
        .await
        .unwrap()
        .header("Content-Length", 2)
        .body(body())
        .send()
        .await
        .unwrap_err();
    assert!(error.is_timeout());
}
