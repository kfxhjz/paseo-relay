use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use paseo_relay::{serve, Config, ServerHandle};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn test_config() -> Config {
    Config {
        host: "127.0.0.1".parse().unwrap(),
        port: 0,
        shutdown_grace: Duration::from_millis(50),
        session_idle: Duration::from_millis(200),
        data_attach_timeout: Duration::from_millis(200),
        delivery_timeout: Duration::from_millis(2_000),
        ..Config::default()
    }
}

async fn start(config: Config) -> (ServerHandle, std::net::SocketAddr) {
    let handle = serve(config).await.expect("server starts");
    let addr = handle.local_addr();
    (handle, addr)
}

async fn connect(addr: std::net::SocketAddr, query: &str) -> Client {
    let url = format!("ws://{addr}/ws?{query}");
    let (client, response) = connect_async(url).await.expect("websocket connects");
    assert_eq!(response.status(), 101);
    client
}

async fn next(ws: &mut Client) -> Message {
    tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("message timeout")
        .expect("stream ended")
        .expect("websocket error")
}

async fn expect_text(ws: &mut Client) -> String {
    match next(ws).await {
        Message::Text(text) => text.as_str().to_owned(),
        other => panic!("expected text, got {other:?}"),
    }
}

async fn expect_close(ws: &mut Client) -> (u16, String) {
    loop {
        match next(ws).await {
            Message::Close(frame) => {
                let (code, reason) = match frame {
                    Some(frame) => (frame.code.into(), frame.reason.to_string()),
                    None => (1005, String::new()),
                };
                return (code, reason);
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected close, got {other:?}"),
        }
    }
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect http");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await.unwrap();
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = text.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
    (status, body)
}

async fn raw_bytes_http(addr: std::net::SocketAddr, request: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect http");
    stream.write_all(request).await.unwrap();
    let mut buffer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buffer)).await;
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, text)
}

/// Raw WebSocket upgrade returning the underlying stream (used to send
/// hand-crafted frames that tungstenite cannot express).
async fn raw_ws(addr: std::net::SocketAddr, query: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect ws");
    let request = upgrade_request(
        addr,
        &format!("/ws?{query}"),
        Some("dGhlIHNhbXBsZSBub25jZQ=="),
    )
    .await;
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0u8; 1024];
        let read = stream
            .read(&mut chunk)
            .await
            .expect("read upgrade response");
        assert!(read > 0, "upgrade response ended early");
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    assert!(text.starts_with("HTTP/1.1 101"), "upgrade failed: {text}");
    stream
}

/// Masked client frame header without a payload (client frames must be masked).
fn masked_frame_header(opcode: u8, length: usize, mask: [u8; 4]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    if length <= 125 {
        out.push(0x80 | length as u8);
    } else if length <= usize::from(u16::MAX) {
        out.push(0x80 | 126);
        out.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(length as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out
}

/// Like http_get, but tolerates a connection reset: the TCP-capacity path writes
/// a 503 and closes without draining the request, which can produce an RST.
async fn lenient_http_status(addr: std::net::SocketAddr, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect(addr).await.ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut buffer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buffer)).await;
    let text = String::from_utf8_lossy(&buffer);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
}

async fn raw_http(addr: std::net::SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect http");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buffer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buffer)).await;
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, text)
}

async fn upgrade_request(addr: std::net::SocketAddr, path: &str, key: Option<&str>) -> String {
    let key_header = key
        .map(|key| format!("Sec-WebSocket-Key: {key}\r\n"))
        .unwrap_or_default();
    format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade, close\r\nUpgrade: websocket\r\n{key_header}Sec-WebSocket-Version: 13\r\n\r\n"
    )
}

#[tokio::test]
async fn health_ready_and_metrics() {
    let (server, addr) = start(test_config()).await;
    let (status, body) = http_get(addr, "/health").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"status\":\"ok\""));
    let (status, body) = http_get(addr, "/ready").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"status\":\"ready\""));
    let (status, body) = http_get(addr, "/metrics").await;
    assert_eq!(status, 200);
    assert!(body.contains("paseo_relay_ready 1"));
    assert!(body.contains("paseo_relay_active_websockets 0"));
    assert!(body.contains("paseo_relay_ingress_budget_bytes 536870912"));
    let (status, _) = http_get(addr, "/nope").await;
    assert_eq!(status, 404);
    server.shutdown().await;
}

#[tokio::test]
async fn v1_forwards_frames_both_directions() {
    let (server, addr) = start(test_config()).await;
    let mut server_ws = connect(addr, "serverId=srv_v1&role=server").await;
    let mut client_ws = connect(addr, "serverId=srv_v1&role=client").await;

    client_ws
        .send(Message::Text("hello-daemon".into()))
        .await
        .unwrap();
    assert_eq!(expect_text(&mut server_ws).await, "hello-daemon");

    server_ws
        .send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    match next(&mut client_ws).await {
        Message::Binary(payload) => assert_eq!(payload.as_ref(), &[1, 2, 3]),
        other => panic!("expected binary, got {other:?}"),
    }
    server.shutdown().await;
}

#[tokio::test]
async fn v2_control_data_and_notifications() {
    let (server, addr) = start(test_config()).await;
    let mut control = connect(addr, "serverId=srv_v2&role=server&v=2").await;
    let sync: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(sync["type"], "sync");
    assert_eq!(sync["connectionIds"], serde_json::json!([]));

    let mut client = connect(addr, "serverId=srv_v2&role=client&v=2").await;
    let connected: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(connected["type"], "connected");
    let connection_id = connected["connectionId"].as_str().unwrap().to_owned();
    assert!(connection_id.starts_with("conn_"));

    let mut data = connect(
        addr,
        &format!("serverId=srv_v2&role=server&v=2&connectionId={connection_id}"),
    )
    .await;

    client
        .send(Message::Text("to-daemon".into()))
        .await
        .unwrap();
    assert_eq!(expect_text(&mut data).await, "to-daemon");

    data.send(Message::Text("to-client".into())).await.unwrap();
    assert_eq!(expect_text(&mut client).await, "to-client");

    client.close(None).await.unwrap();
    let disconnected: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(disconnected["type"], "disconnected");
    assert_eq!(disconnected["connectionId"], connection_id);

    let (code, reason) = expect_close(&mut data).await;
    assert_eq!((code, reason.as_str()), (1001, "Client disconnected"));
    server.shutdown().await;
}

#[tokio::test]
async fn v2_control_ping_gets_pong() {
    let (server, addr) = start(test_config()).await;
    let mut control = connect(addr, "serverId=srv_ping&role=server&v=2").await;
    let _sync = expect_text(&mut control).await;
    control
        .send(Message::Text(r#"{"type":"ping"}"#.into()))
        .await
        .unwrap();
    let pong: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(pong["type"], "pong");
    assert!(pong["ts"].is_u64());
    server.shutdown().await;
}

#[tokio::test]
async fn replacement_closes_old_socket() {
    let (server, addr) = start(test_config()).await;
    let mut first = connect(addr, "serverId=srv_replace&role=server&v=2&connectionId=c1").await;
    let mut second = connect(addr, "serverId=srv_replace&role=server&v=2&connectionId=c1").await;
    let (code, reason) = expect_close(&mut first).await;
    assert_eq!(code, 1008);
    assert_eq!(reason, "Replaced by new connection");
    second
        .send(Message::Text("still-open".into()))
        .await
        .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn invalid_handshake_key_closes_1008() {
    let (server, addr) = start(test_config()).await;
    let mut client = connect(addr, "serverId=srv_hs&role=client").await;
    client
        .send(Message::Text(
            r#"{"type":"e2ee_hello","key":"AAAA"}"#.into(),
        ))
        .await
        .unwrap();
    let (code, reason) = expect_close(&mut client).await;
    assert_eq!(code, 1008);
    assert!(reason.contains("Invalid handshake key"));
    let (_, metrics) = http_get(addr, "/metrics").await;
    assert!(metrics.contains("paseo_relay_handshake_rejected_total 1"));
    server.shutdown().await;
}

#[tokio::test]
async fn failed_upgrade_does_not_create_a_session() {
    let (server, addr) = start(test_config()).await;
    let request = upgrade_request(addr, "/ws?serverId=no-key&role=client", None).await;
    let (status, _) = raw_http(addr, &request).await;
    assert_eq!(status, 400);
    let request =
        upgrade_request(addr, "/ws?serverId=bad-key&role=client", Some("not-base64")).await;
    let (status, _) = raw_http(addr, &request).await;
    assert_eq!(status, 400);

    let (_, metrics) = http_get(addr, "/metrics").await;
    assert!(metrics.contains("paseo_relay_active_sessions 0"));
    assert!(metrics.contains("paseo_relay_active_websockets 0"));
    server.shutdown().await;
}

#[tokio::test]
async fn invalid_utf8_connection_id_is_rejected() {
    let (server, addr) = start(test_config()).await;
    let request = upgrade_request(
        addr,
        "/ws?serverId=utf8&role=client&v=2&connectionId=%FF",
        Some("dGhlIHNhbXBsZSBub25jZQ=="),
    )
    .await;
    let (status, response) = raw_http(addr, &request).await;
    assert_eq!(status, 400, "response: {response}");
    server.shutdown().await;
}

#[tokio::test]
async fn oversized_frame_closes_1009() {
    let mut config = test_config();
    config.max_message_bytes = 1024;
    config.ingress_budget_bytes = 128 * 1024 * 1024;
    let (server, addr) = start(config).await;
    let mut client = connect(addr, "serverId=srv_big&role=client").await;
    client
        .send(Message::Binary(vec![0u8; 2048].into()))
        .await
        .unwrap();
    let (code, _) = expect_close(&mut client).await;
    assert_eq!(code, 1009);
    server.shutdown().await;
}

#[tokio::test]
async fn websocket_capacity_returns_503() {
    let mut config = test_config();
    config.max_connections = 1;
    let (server, addr) = start(config).await;
    let _first = connect(addr, "serverId=cap&role=server").await;
    let url = format!("ws://{addr}/ws?serverId=cap&role=client");
    let error = connect_async(url)
        .await
        .expect_err("second upgrade rejected");
    let status = match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("unexpected error: {other:?}"),
    };
    assert_eq!(status, 503);
    server.shutdown().await;
}

#[tokio::test]
async fn per_ip_capacity_returns_429() {
    let mut config = test_config();
    config.per_ip_max_connections = 1;
    let (server, addr) = start(config).await;
    let _first = connect(addr, "serverId=ip1&role=server").await;
    let url = format!("ws://{addr}/ws?serverId=ip2&role=server");
    let error = connect_async(url)
        .await
        .expect_err("second ip upgrade rejected");
    let status = match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("unexpected error: {other:?}"),
    };
    assert_eq!(status, 429);
    server.shutdown().await;
}

#[tokio::test]
async fn drain_closes_websockets_with_1012() {
    let (server, addr) = start(test_config()).await;
    let mut ws = connect(addr, "serverId=drain&role=server").await;
    let handle = tokio::spawn(async move { server.shutdown().await });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let (code, reason) = expect_close(&mut ws).await;
    assert_eq!(code, 1012);
    assert_eq!(reason, "Relay shutting down");
    handle.await.unwrap();
}

#[tokio::test]
async fn metrics_count_forwarded_frames() {
    let (server, addr) = start(test_config()).await;
    let mut daemon = connect(addr, "serverId=srv_metrics&role=server").await;
    let mut client = connect(addr, "serverId=srv_metrics&role=client").await;
    client.send(Message::Text("one".into())).await.unwrap();
    assert_eq!(expect_text(&mut daemon).await, "one");
    client.send(Message::Text("two".into())).await.unwrap();
    assert_eq!(expect_text(&mut daemon).await, "two");
    let (_, metrics) = http_get(addr, "/metrics").await;
    assert!(metrics.contains("paseo_relay_frames_forwarded_total 2"));
    server.shutdown().await;
}

#[tokio::test]
async fn missing_data_route_closes_1013() {
    let (server, addr) = start(test_config()).await;
    let mut client = connect(
        addr,
        "serverId=srv_missing&role=client&v=2&connectionId=does-not-exist",
    )
    .await;
    client.send(Message::Text("lost".into())).await.unwrap();
    let (code, reason) = expect_close(&mut client).await;
    assert_eq!(code, 1013);
    assert_eq!(reason, "Data route unavailable");
    server.shutdown().await;
}

#[tokio::test]
async fn client_frame_waits_for_data_socket() {
    let mut config = test_config();
    config.data_attach_timeout = Duration::from_millis(1_000);
    let (server, addr) = start(config).await;
    let mut control = connect(addr, "serverId=srv_wait&role=server&v=2").await;
    let _sync = expect_text(&mut control).await;
    let mut client = connect(addr, "serverId=srv_wait&role=client&v=2").await;
    let connected: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    let connection_id = connected["connectionId"].as_str().unwrap().to_owned();

    client
        .send(Message::Text("early-frame".into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut data = connect(
        addr,
        &format!("serverId=srv_wait&role=server&v=2&connectionId={connection_id}"),
    )
    .await;
    assert_eq!(expect_text(&mut data).await, "early-frame");
    server.shutdown().await;
}

#[tokio::test]
async fn control_sync_lists_existing_clients() {
    let (server, addr) = start(test_config()).await;
    let _client = connect(addr, "serverId=srv_sync&role=client&v=2").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut control = connect(addr, "serverId=srv_sync&role=server&v=2").await;
    let sync: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(sync["type"], "sync");
    assert_eq!(sync["connectionIds"].as_array().unwrap().len(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn client_limit_per_connection_is_enforced() {
    let mut config = test_config();
    config.max_clients_per_connection = 1;
    let (server, addr) = start(config).await;
    let _control = connect(
        addr,
        "serverId=srv_client_limit&role=server&v=2&connectionId=shared",
    )
    .await;
    let _first = connect(
        addr,
        "serverId=srv_client_limit&role=client&v=2&connectionId=shared",
    )
    .await;
    let mut second = connect(
        addr,
        "serverId=srv_client_limit&role=client&v=2&connectionId=shared",
    )
    .await;
    let (code, _) = expect_close(&mut second).await;
    assert_eq!(code, 1013);
    server.shutdown().await;
}

#[tokio::test]
async fn ingress_budget_rejects_excess_sources() {
    let mut config = test_config();
    config.max_message_bytes = 1024;
    // Must admit one maximum message including chunk bookkeeping; direct test
    // configs bypass the env parser's 128 MiB floor.
    config.ingress_budget_bytes = 205_000;
    config.ingress_wait_timeout = Duration::from_millis(50);
    config.data_attach_timeout = Duration::from_millis(500);
    let (server, addr) = start(config).await;

    // Each client waits for a data route that never appears, holding one
    // ingress reservation. The first few fit; a later one times out with 1013.
    let mut clients = Vec::new();
    for index in 0..64 {
        let mut client = connect(
            addr,
            &format!("serverId=srv_budget&role=client&v=2&connectionId=missing-{index}"),
        )
        .await;
        client
            .send(Message::Binary(vec![0u8; 1024].into()))
            .await
            .unwrap();
        clients.push(client);
    }

    let mut rejected = 0;
    for client in &mut clients {
        if let Ok(Some(Ok(Message::Close(Some(frame))))) =
            tokio::time::timeout(Duration::from_millis(800), client.next()).await
        {
            if u16::from(frame.code) == 1013 {
                rejected += 1;
            }
        }
    }
    assert!(rejected > 0, "expected at least one capacity close");
    server.shutdown().await;
}

#[tokio::test]
async fn valid_handshake_key_is_forwarded() {
    let (server, addr) = start(test_config()).await;
    let mut daemon = connect(addr, "serverId=srv_key&role=server").await;
    let mut client = connect(addr, "serverId=srv_key&role=client").await;
    // X25519 base point (u = 9), canonical little-endian 32-byte encoding.
    let hello = r#"{"type":"e2ee_hello","key":"CQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#;
    client.send(Message::Text(hello.into())).await.unwrap();
    assert_eq!(expect_text(&mut daemon).await, hello);
    server.shutdown().await;
}

#[tokio::test]
async fn session_capacity_is_enforced_but_existing_server_id_still_upgrades() {
    let mut config = test_config();
    config.max_sessions = 1;
    let (server, addr) = start(config).await;
    let _first = connect(addr, "serverId=session-one&role=server").await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    let url = format!("ws://{addr}/ws?serverId=session-two&role=client");
    let error = connect_async(url).await.expect_err("new session rejected");
    let status = match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("unexpected error: {other:?}"),
    };
    assert_eq!(status, 503);

    // Existing sessions remain attachable even when the session map is full.
    let _second = connect(addr, "serverId=session-one&role=client").await;
    server.shutdown().await;
}

#[tokio::test]
async fn oversized_server_id_is_rejected() {
    let (server, addr) = start(test_config()).await;
    let id = "x".repeat(257);
    let url = format!("ws://{addr}/ws?serverId={id}&role=server");
    let error = connect_async(url).await.expect_err("oversized id rejected");
    let status = match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("unexpected error: {other:?}"),
    };
    assert_eq!(status, 400);
    server.shutdown().await;
}

#[tokio::test]
async fn stalled_frame_payload_is_closed_1013() {
    let mut config = test_config();
    config.frame_read_timeout = Duration::from_millis(100);
    let (server, addr) = start(config).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET /ws?serverId=stalled&role=client HTTP/1.1\r\nHost: {addr}\r\n\
         Connection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        if read == 0 {
            panic!("server closed before upgrade response");
        }
        response.extend_from_slice(&chunk[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 101"));

    // A masked text frame claiming 1024 bytes, followed by only 8 payload bytes.
    let mut frame = vec![0x81, 0xFE, 0x04, 0x00, 0x01, 0x02, 0x03, 0x04];
    frame.extend_from_slice(&[0u8; 8]);
    stream.write_all(&frame).await.unwrap();

    let mut received = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
        .await
        .expect("close timeout")
        .unwrap();
    assert!(read > 0, "expected close frame bytes");
    assert!(
        received.windows(2).any(|window| window == [0x03, 0xF5]),
        "expected close code 1013, got {received:?}"
    );
    server.shutdown().await;
}

async fn upgrade_status(addr: std::net::SocketAddr, path: &str, origin: Option<&str>) -> u16 {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let origin = origin
        .map(|value| format!("Origin: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{origin}\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
            .await
            .expect("upgrade response timeout")
            .unwrap();
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn origin_allowlist_allows_native_clients_and_blocks_other_browsers() {
    let mut config = test_config();
    config.allowed_origins = vec!["https://app.paseo.sh".to_owned()];
    let (server, addr) = start(config).await;

    // Native clients do not send Origin.
    let mut native = connect(addr, "serverId=origin&role=server").await;
    assert_eq!(
        upgrade_status(addr, "/ws?serverId=origin2&role=server", None).await,
        101
    );
    assert_eq!(
        upgrade_status(
            addr,
            "/ws?serverId=origin3&role=server",
            Some("https://evil.test")
        )
        .await,
        403
    );
    assert_eq!(
        upgrade_status(
            addr,
            "/ws?serverId=origin4&role=server",
            Some("https://app.paseo.sh")
        )
        .await,
        101
    );
    let _ = native.close(None).await;
    server.shutdown().await;
}

#[tokio::test]
async fn fragmented_message_with_only_pings_times_out() {
    let mut config = test_config();
    config.message_read_timeout = Duration::from_millis(300);
    config.frame_read_timeout = Duration::from_secs(2);
    let (server, addr) = start(config).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET /ws?serverId=fragment-stall&role=client HTTP/1.1\r\nHost: {addr}\r\n\
         Connection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "server closed before 101");
        response.extend_from_slice(&chunk[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 101"));

    // fin=0 text fragment "x" (masked client frame).
    let fragment = [0x01u8, 0x81, 0x00, 0x00, 0x00, 0x00, b'x'];
    stream.write_all(&fragment).await.unwrap();
    // Keep the connection busy with valid ping frames; the fragmented-message
    // deadline must still fire.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        // masked empty ping; the server may close mid-loop on the message
        // deadline, which is the behavior under test.
        if stream
            .write_all(&[0x89u8, 0x80, 0x00, 0x00, 0x00, 0x00])
            .await
            .is_err()
        {
            break;
        }
    }

    let mut received = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut received)).await;
    assert!(
        received.windows(2).any(|window| window == [0x03, 0xF5]),
        "expected close code 1013, got {received:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn control_watchdog_nudges_then_closes_1011() {
    let mut config = test_config();
    config.data_attach_timeout = Duration::from_secs(5);
    config.control_nudge_after = Duration::from_millis(100);
    config.control_unresponsive_after = Duration::from_millis(350);
    let (server, addr) = start(config).await;

    let mut control = connect(addr, "serverId=srv_watchdog&role=server&v=2").await;
    let _first_sync = expect_text(&mut control).await;
    let mut client = connect(
        addr,
        "serverId=srv_watchdog&role=client&v=2&connectionId=missing",
    )
    .await;
    let connected: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(connected["type"], "connected");
    client.send(Message::Text("waiting".into())).await.unwrap();

    let nudged = expect_text(&mut control).await;
    let nudged: Value = serde_json::from_str(&nudged).unwrap();
    assert_eq!(nudged["type"], "sync");
    let (code, reason) = expect_close(&mut control).await;
    assert_eq!(code, 1011);
    assert_eq!(reason, "Control unresponsive");
    let _ = client.close(None).await;
    server.shutdown().await;
}

#[tokio::test]
async fn client_limit_reports_client_capacity() {
    let mut config = test_config();
    config.max_clients_per_connection = 1;
    let (server, addr) = start(config).await;
    let _control = connect(addr, "serverId=srv_client_cap&role=server&v=2").await;
    let _first = connect(
        addr,
        "serverId=srv_client_cap&role=client&v=2&connectionId=shared",
    )
    .await;
    let mut second = connect(
        addr,
        "serverId=srv_client_cap&role=client&v=2&connectionId=shared",
    )
    .await;
    let (code, reason) = expect_close(&mut second).await;
    assert_eq!(code, 1013);
    assert_eq!(reason, "Relay client capacity");
    server.shutdown().await;
}

#[tokio::test]
async fn control_watchdog_does_not_truncate_data_wait() {
    let mut config = test_config();
    config.data_attach_timeout = Duration::from_secs(2);
    config.control_nudge_after = Duration::from_millis(100);
    config.control_unresponsive_after = Duration::from_millis(400);
    let (server, addr) = start(config).await;

    let mut control = connect(addr, "serverId=srv_watchdog_wait&role=server&v=2").await;
    let _first_sync = expect_text(&mut control).await;
    let mut client = connect(
        addr,
        "serverId=srv_watchdog_wait&role=client&v=2&connectionId=missing",
    )
    .await;
    let connected: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(connected["type"], "connected");
    client
        .send(Message::Text("waiting-for-data".into()))
        .await
        .unwrap();

    // The independent watchdog nudges and then closes the control socket, but
    // must not shorten the client's data-attach wait.
    let nudged: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(nudged["type"], "sync");
    let (code, reason) = expect_close(&mut control).await;
    assert_eq!((code, reason.as_str()), (1011, "Control unresponsive"));

    // Data attaches well after the control watchdog, still inside the client's
    // DATA_ATTACH_TIMEOUT window; the already-sent frame must be delivered.
    let mut data = connect(
        addr,
        "serverId=srv_watchdog_wait&role=server&v=2&connectionId=missing",
    )
    .await;
    assert_eq!(expect_text(&mut data).await, "waiting-for-data");
    let _ = client.close(None).await;
    server.shutdown().await;
}

#[tokio::test]
async fn drain_closes_a_socket_waiting_for_data() {
    let mut config = test_config();
    config.shutdown_grace = Duration::from_millis(200);
    config.data_attach_timeout = Duration::from_secs(5);
    let (server, addr) = start(config).await;
    let mut client = connect(
        addr,
        "serverId=drain-wait&role=client&v=2&connectionId=missing",
    )
    .await;
    client.send(Message::Text("waiting".into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let handle = tokio::spawn(async move { server.shutdown().await });
    let (code, reason) = expect_close(&mut client).await;
    assert_eq!((code, reason.as_str()), (1012, "Relay shutting down"));
    handle.await.unwrap();
}

#[tokio::test]
async fn healthcheck_subcommand_probes_running_relay() {
    let (server, addr) = start(test_config()).await;
    let port = addr.port().to_string();
    let bin = env!("CARGO_BIN_EXE_paseo-relay");
    let output = tokio::process::Command::new(bin)
        .arg("healthcheck")
        .env("PASEO_RELAY_PORT", &port)
        .output()
        .await
        .expect("run healthcheck");
    assert!(
        output.status.success(),
        "healthcheck failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.shutdown().await;
}

#[tokio::test]
async fn version_endpoint_reports_build() {
    let (server, addr) = start(test_config()).await;
    let (status, body) = http_get(addr, "/version").await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert!(!value["version"].as_str().unwrap().is_empty());
    assert!(!value["revision"].as_str().unwrap().is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn stalled_frame_header_does_not_reserve_the_whole_budget() {
    // Regression for the audit finding: a frame header declaring a maximum
    // payload must not reserve the full message before payload bytes arrive.
    let mut config = test_config();
    config.max_message_bytes = 32 * 1024;
    // One maximum message costs 132,096 + chunk bookkeeping; leave headroom for
    // the victim's small frame.
    config.ingress_budget_bytes = 330_000;
    config.ingress_wait_timeout = Duration::from_millis(200);
    let (server, addr) = start(config).await;

    let mut attacker = raw_ws(addr, "serverId=srv_stalled&role=client&v=1").await;
    attacker
        .write_all(&masked_frame_header(
            0x2,
            32 * 1024,
            [0x11, 0x22, 0x33, 0x44],
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut daemon = connect(addr, "serverId=srv_legit&role=server&v=1").await;
    let mut client = connect(addr, "serverId=srv_legit&role=client&v=1").await;
    client
        .send(Message::Text("still-works".into()))
        .await
        .unwrap();
    let received = next(&mut daemon).await;
    assert_eq!(received, Message::Text("still-works".into()));

    drop(attacker);
    server.shutdown().await;
}

#[tokio::test]
async fn oversized_preupgrade_headers_are_rejected() {
    let (server, addr) = start(test_config()).await;
    let mut request = b"GET /health HTTP/1.1\r\nHost: x\r\nX-Fill: ".to_vec();
    request.extend(std::iter::repeat_n(b'a', 20 * 1024));
    let (status, text) = raw_bytes_http(addr, &request).await;
    assert_eq!(status, 431, "expected 431, got {text:?}");
    server.shutdown().await;
}

#[tokio::test]
async fn unparseable_origin_is_rejected_when_allowlist_configured() {
    let mut config = test_config();
    config.allowed_origins = vec!["https://app.example".to_owned()];
    let (server, addr) = start(config).await;

    let mut request = b"GET /ws?serverId=srv_origin&role=client HTTP/1.1\r\nHost: x\r\n\
        Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
        Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nOrigin: "
        .to_vec();
    request.push(0xFF);
    request.extend_from_slice(b"\r\n\r\n");
    let (status, _text) = raw_bytes_http(addr, &request).await;
    assert_eq!(status, 403);
    server.shutdown().await;
}

#[tokio::test]
async fn tcp_slot_is_held_for_upgraded_sockets() {
    // Regression for the audit finding: upgrading a connection must not release
    // the TCP slot, so MAX_TCP_CONNECTIONS bounds all open sockets.
    let mut config = test_config();
    config.max_connections = 1;
    config.max_sessions = 4;
    config.max_tcp_connections = 1;
    let (server, addr) = start(config).await;

    let ws = connect(addr, "serverId=srv_slot&role=server&v=1").await;
    let denied = lenient_http_status(addr, "/health").await;
    assert_ne!(
        denied,
        Some(200),
        "an upgraded socket must still hold its TCP slot (got {denied:?})"
    );

    drop(ws);
    let mut status = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(value) = lenient_http_status(addr, "/health").await {
            status = Some(value);
            if value == 200 {
                break;
            }
        }
    }
    assert_eq!(
        status,
        Some(200),
        "TCP slot must be released when the socket ends"
    );
    server.shutdown().await;
}
