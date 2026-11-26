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
    // These 1 KiB messages would otherwise all fit in the small reserve.
    config.ingress_small_reserve_bytes = 0;
    config.ingress_wait_timeout = Duration::from_millis(50);
    // Longer than the collection window below, so a 1013 seen there is the
    // budget, not "Data route unavailable".
    config.data_attach_timeout = Duration::from_secs(5);
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
            if u16::from(frame.code) == 1013 && frame.reason.as_str() == "Relay ingress capacity" {
                rejected += 1;
            }
        }
    }
    assert!(rejected > 0, "expected at least one capacity close");
    let (_, metrics) = http_get(addr, "/metrics").await;
    assert!(
        !metrics.contains("paseo_relay_ingress_rejections_total 0"),
        "budget closes must be counted"
    );
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

// ---------------------------------------------------------------------------
// Regression tests for the 2026-11 review findings.
// ---------------------------------------------------------------------------

/// Masked frame header with an all-zero mask key, so payload bytes can be
/// written as they are.
fn frame_header(fin: bool, opcode: u8, length: usize) -> Vec<u8> {
    let mut out = masked_frame_header(opcode, length, [0, 0, 0, 0]);
    if !fin {
        out[0] &= 0x7F;
    }
    out
}

async fn write_zeros(stream: &mut TcpStream, mut bytes: usize) {
    let block = vec![0u8; 256 * 1024];
    while bytes > 0 {
        let step = bytes.min(block.len());
        stream.write_all(&block[..step]).await.unwrap();
        bytes -= step;
    }
    stream.flush().await.unwrap();
}

/// Send one non-final binary fragment of `bytes` and then go silent, keeping
/// the socket open: an in-progress message that never finishes.
async fn park_unfinished_fragment(addr: std::net::SocketAddr, id: &str, bytes: usize) -> TcpStream {
    let mut stream = raw_ws(addr, &format!("serverId={id}&role=client")).await;
    stream
        .write_all(&frame_header(false, 0x2, bytes))
        .await
        .unwrap();
    write_zeros(&mut stream, bytes).await;
    stream
}

async fn metric(addr: std::net::SocketAddr, name: &str) -> u64 {
    let (_, body) = http_get(addr, "/metrics").await;
    body.lines()
        .find(|line| line.split_whitespace().next() == Some(name))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("metric {name} missing"))
}

async fn wait_for_metric(addr: std::net::SocketAddr, name: &str, want: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let value = metric(addr, name).await;
        if value == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{name} stayed at {value}, expected {want}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Close code and reason the relay sent on a raw socket, if any.
async fn raw_close(stream: &mut TcpStream, wait: Duration) -> Option<(u16, String)> {
    let mut received = Vec::new();
    let _ = tokio::time::timeout(wait, stream.read_to_end(&mut received)).await;
    parse_close(&received)
}

/// First close frame in bytes received from the relay.
fn parse_close(received: &[u8]) -> Option<(u16, String)> {
    let at = received.iter().position(|byte| *byte == 0x88)?;
    let length = usize::from(*received.get(at + 1)?);
    let payload = received.get(at + 2..at + 2 + length)?;
    let code = u16::from_be_bytes([*payload.first()?, *payload.get(1)?]);
    Some((code, String::from_utf8_lossy(&payload[2..]).into_owned()))
}

async fn next_within(ws: &mut Client, wait: Duration) -> Option<Message> {
    match tokio::time::timeout(wait, ws.next()).await {
        Ok(Some(Ok(message))) => Some(message),
        _ => None,
    }
}

/// Wait for a binary message of `filler` bytes repeated `length` times.
/// Panics with a short description instead of dumping the payload.
async fn expect_binary(ws: &mut Client, wait: Duration, filler: u8, length: usize, what: &str) {
    match next_within(ws, wait).await {
        Some(Message::Binary(payload)) => {
            assert_eq!(payload.len(), length, "{what}: wrong length");
            assert!(
                payload.iter().all(|byte| *byte == filler),
                "{what}: payload corrupted"
            );
        }
        Some(Message::Close(frame)) => panic!("{what}: closed instead ({frame:?})"),
        Some(other) => panic!("{what}: unexpected {} frame", message_kind(&other)),
        None => panic!("{what}: nothing arrived within {wait:?}"),
    }
}

fn message_kind(message: &Message) -> &'static str {
    match message {
        Message::Text(_) => "text",
        Message::Binary(_) => "binary",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::Close(_) => "close",
        Message::Frame(_) => "raw",
    }
}

/// A small relay whose bulk budget is exactly one maximum (256 KiB) message.
fn tight_budget_config() -> Config {
    let mut config = test_config();
    config.max_message_bytes = 256 * 1024;
    config.ingress_budget_bytes = config.max_message_weighted_charge();
    config.ingress_small_reserve_bytes = 0;
    config.ingress_wait_timeout = Duration::from_secs(2);
    config
}

/// Fill `tight_budget_config`'s bulk budget to the last byte with two
/// unfinished fragments. Returns the large and the small holder.
async fn pin_tight_budget(addr: std::net::SocketAddr, budget: u64) -> (TcpStream, TcpStream) {
    // 1024 + 32 * 32,960 weighted.
    let large = park_unfinished_fragment(addr, "pin-large", 256 * 1024).await;
    // 1024 + 5 * 32,960 + (7,648 * 4 + 192) weighted: the remainder.
    let small = park_unfinished_fragment(addr, "pin-small", 5 * 8192 + 7648).await;
    wait_for_metric(addr, "paseo_relay_ingress_reserved_bytes", budget).await;
    (large, small)
}

#[tokio::test]
async fn small_frame_reclaims_a_much_larger_unfinished_message() {
    let config = tight_budget_config();
    let budget = config.ingress_budget_bytes;
    let (server, addr) = start(config).await;
    let (mut large, _small) = pin_tight_budget(addr, budget).await;

    // The budget is full and its holders are fresh, so nothing counts as
    // stalled for another two seconds. A small frame must still get through
    // before that, by displacing the large message once it has waited half of
    // its own 2 s reservation wait.
    let mut daemon = connect(addr, "serverId=victim&role=server").await;
    let mut client = connect(addr, "serverId=victim&role=client").await;
    client.send(Message::Text("hi".into())).await.unwrap();
    assert_eq!(
        next_within(&mut daemon, Duration::from_millis(1800)).await,
        Some(Message::Text("hi".into())),
        "small frame must be delivered while large messages hold the budget"
    );

    assert_eq!(
        raw_close(&mut large, Duration::from_secs(2)).await,
        Some((1013, "Relay ingress reclaimed".to_owned()))
    );
    assert!(metric(addr, "paseo_relay_ingress_reclaims_total").await >= 1);
    server.shutdown().await;
}

#[tokio::test]
async fn stalled_message_is_reclaimed_for_a_large_transfer() {
    let config = tight_budget_config();
    let budget = config.ingress_budget_bytes;
    let (server, addr) = start(config).await;
    let (mut large, _small) = pin_tight_budget(addr, budget).await;

    // Long enough for both holders to count as stalled.
    tokio::time::sleep(Duration::from_millis(2300)).await;

    let mut daemon = connect(addr, "serverId=victim&role=server").await;
    let mut client = connect(addr, "serverId=victim&role=client").await;
    client
        .send(Message::Binary(vec![7u8; 100 * 1024].into()))
        .await
        .unwrap();
    expect_binary(
        &mut daemon,
        Duration::from_secs(3),
        7,
        100 * 1024,
        "a large transfer must displace stalled messages",
    )
    .await;
    assert_eq!(
        raw_close(&mut large, Duration::from_secs(2)).await,
        Some((1013, "Relay ingress reclaimed".to_owned()))
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_message_trickled_to_keep_its_budget_yields_to_a_newer_transfer() {
    // A sender can avoid ever looking stalled by trickling. Budget it has
    // held for longer than INGRESS_RECLAIM_AFTER must still give way.
    let mut config = test_config();
    config.max_message_bytes = 1024 * 1024;
    config.ingress_budget_bytes = config.max_message_weighted_charge();
    config.ingress_small_reserve_bytes = 0;
    // Only a message that receives nothing at all counts as slow here, so
    // this test cannot pass through the stalled/slow rule.
    config.ingress_reclaim_min_rate_bytes = 0;
    config.ingress_reclaim_after = Duration::from_secs(5);
    config.ingress_wait_timeout = Duration::from_secs(2);
    let (server, addr) = start(config).await;

    // 850 KiB up front, then a few bytes five times a second for as long as
    // the relay lets it: always making progress, never finishing. The burst
    // is charged 3,503,168 of the 4,416,512 weighted bytes, which leaves less
    // than the 1,237,120 a 300 KiB message needs.
    let holder = park_unfinished_fragment(addr, "trickle", 850 * 1024).await;
    let (mut holder_read, mut holder_write) = holder.into_split();
    let trickle = tokio::spawn(async move {
        let mut frame = frame_header(false, 0x0, 16);
        frame.extend_from_slice(&[0u8; 16]);
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if holder_write.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    let holder_close = tokio::spawn(async move {
        let mut received = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(12),
            holder_read.read_to_end(&mut received),
        )
        .await;
        parse_close(&received)
    });

    // Once the holder is past its allowance, a 300 KiB transfer arrives. Not
    // enough budget is left for it, so it has to wait.
    tokio::time::sleep(Duration::from_millis(5400)).await;
    // Nothing has been reclaimed so far: without a waiter, a message may take
    // as long as it likes.
    assert_eq!(metric(addr, "paseo_relay_ingress_reclaims_total").await, 0);
    let mut daemon = connect(addr, "serverId=victim&role=server").await;
    let mut client = connect(addr, "serverId=victim&role=client").await;
    client
        .send(Message::Binary(vec![5u8; 300 * 1024].into()))
        .await
        .unwrap();
    expect_binary(
        &mut daemon,
        Duration::from_secs(4),
        5,
        300 * 1024,
        "the newer transfer must be served once the old message has had its time",
    )
    .await;

    assert_eq!(
        holder_close.await.unwrap(),
        Some((1013, "Relay ingress reclaimed".to_owned()))
    );
    trickle.abort();
    assert!(metric(addr, "paseo_relay_ingress_reclaims_total").await >= 1);
    assert_eq!(
        metric(addr, "paseo_relay_ingress_rejections_total").await,
        0
    );
    server.shutdown().await;
}

#[tokio::test]
async fn small_reserve_serves_small_frames_while_the_bulk_budget_is_exhausted() {
    let mut config = tight_budget_config();
    config.ingress_small_reserve_bytes = Config::default().ingress_small_reserve_bytes;
    config.ingress_wait_timeout = Duration::from_millis(300);
    config.data_attach_timeout = Duration::from_secs(5);
    let (server, addr) = start(config).await;

    // A complete 256 KiB message waiting for a data route holds most of the
    // bulk budget, and it is not reclaimable: it is no longer incomplete.
    let mut waiting = connect(addr, "serverId=bulk&role=client&v=2&connectionId=a").await;
    waiting
        .send(Message::Binary(vec![0u8; 256 * 1024].into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut daemon = connect(addr, "serverId=victim&role=server").await;
    let mut client = connect(addr, "serverId=victim&role=client").await;

    // Another large message cannot be admitted: the bulk pool is exhausted.
    let mut blocked = connect(addr, "serverId=victim2&role=client").await;
    let _blocked_peer = connect(addr, "serverId=victim2&role=server").await;
    blocked
        .send(Message::Binary(vec![0u8; 200 * 1024].into()))
        .await
        .unwrap();
    assert_eq!(
        expect_close(&mut blocked).await,
        (1013, "Relay ingress capacity".to_owned())
    );

    // Small frames are unaffected.
    for round in 0..5 {
        let text = format!("small-{round}");
        client
            .send(Message::Text(text.clone().into()))
            .await
            .unwrap();
        assert_eq!(expect_text(&mut daemon).await, text);
    }
    assert!(metric(addr, "paseo_relay_ingress_small_budget_bytes").await > 0);
    drop(waiting);
    server.shutdown().await;
}

#[tokio::test]
async fn unfinished_fragments_no_longer_starve_other_sessions() {
    // The attack from the review, at production settings with the 129 MiB
    // budget every Compose/Kubernetes example uses: one ~32 MiB unfinished
    // fragment reserves the bulk budget (all but 261,176 weighted bytes, which
    // is less than any message above 64 KiB needs), and a second unfinished
    // fragment used to take the rest.
    let config = Config {
        host: "127.0.0.1".parse().unwrap(),
        port: 0,
        shutdown_grace: Duration::from_millis(50),
        ingress_budget_bytes: 135_266_304,
        ..Config::default()
    };
    let (server, addr) = start(config).await;

    let mut large = park_unfinished_fragment(addr, "atk-large", 33_554_418).await;
    wait_for_metric(addr, "paseo_relay_ingress_reserved_bytes", 135_005_128).await;
    // Non-final fragments are always charged to the bulk pool so a peer cannot
    // park a small opening fragment in the small reserve and follow it with
    // multi-megabyte continuations. Even with bulk filled to the exact ceiling,
    // the small reserve stays untouched and bulk reclamation clears the stalls.
    let _small = park_unfinished_fragment(addr, "atk-small", 64_654).await;
    wait_for_metric(addr, "paseo_relay_ingress_reserved_bytes", 135_266_304).await;
    assert_eq!(
        metric(addr, "paseo_relay_ingress_small_reserved_bytes").await,
        0
    );

    let mut daemon = connect(addr, "serverId=victim&role=server").await;
    let mut client = connect(addr, "serverId=victim&role=client").await;

    // Small frames: served from the reserve at once, the attack is invisible.
    client.send(Message::Text("hi".into())).await.unwrap();
    assert_eq!(
        next_within(&mut daemon, Duration::from_secs(1)).await,
        Some(Message::Text("hi".into()))
    );

    // A large transfer needs the bulk pool: the stalled holders are evicted
    // for it well inside the 5 s reservation wait.
    client
        .send(Message::Binary(vec![3u8; 1024 * 1024].into()))
        .await
        .unwrap();
    expect_binary(
        &mut daemon,
        Duration::from_secs(8),
        3,
        1024 * 1024,
        "large transfer must complete once the stalled messages are reclaimed",
    )
    .await;
    assert_eq!(
        raw_close(&mut large, Duration::from_secs(2)).await,
        Some((1013, "Relay ingress reclaimed".to_owned()))
    );
    server.shutdown().await;
}

#[tokio::test]
async fn stalled_fragment_gap_is_closed_by_the_stall_timeout() {
    let mut config = test_config();
    config.read_stall_timeout = Duration::from_millis(300);
    let (server, addr) = start(config).await;

    // A non-final fragment, then silence: the next fragment never comes.
    let mut gap = park_unfinished_fragment(addr, "gap", 16).await;
    // A frame that stops in the middle of its payload.
    let mut partial = raw_ws(addr, "serverId=partial&role=client").await;
    partial
        .write_all(&frame_header(true, 0x2, 64 * 1024))
        .await
        .unwrap();
    write_zeros(&mut partial, 10 * 1024).await;

    let started = tokio::time::Instant::now();
    assert_eq!(
        raw_close(&mut gap, Duration::from_secs(3)).await,
        Some((1013, "Relay read timeout".to_owned()))
    );
    assert_eq!(
        raw_close(&mut partial, Duration::from_secs(3)).await,
        Some((1013, "Relay read timeout".to_owned()))
    );
    // Far sooner than the 300 s frame / 600 s message deadlines.
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(metric(addr, "paseo_relay_read_timeouts_total").await >= 2);
    server.shutdown().await;
}

#[tokio::test]
async fn session_slot_is_released_as_soon_as_its_last_socket_closes() {
    let mut config = test_config();
    config.max_sessions = 3;
    config.per_ip_max_connections = 1;
    // The idle sweep must play no part in this.
    config.session_idle = Duration::from_secs(3600);
    let (server, addr) = start(config).await;

    // One client, limited to a single concurrent WebSocket, cycling through
    // far more serverIds than there are session slots.
    for index in 0..20 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let url = format!("ws://{addr}/ws?serverId=churn-{index}&role=client&v=2");
            match connect_async(url).await {
                Ok((mut ws, _)) => {
                    let _ = ws.close(None).await;
                    break;
                }
                Err(error) => {
                    // 429 while the previous socket is still being torn down
                    // is expected; a session-capacity 503 never is.
                    if let tokio_tungstenite::tungstenite::Error::Http(response) = &error {
                        assert_eq!(response.status().as_u16(), 429, "connection {index}");
                    }
                    assert!(tokio::time::Instant::now() < deadline, "{error:?}");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }

    wait_for_metric(addr, "paseo_relay_active_websockets", 0).await;
    wait_for_metric(addr, "paseo_relay_active_sessions", 0).await;
    let (ready, _) = http_get(addr, "/ready").await;
    assert_eq!(ready, 200);
    let _daemon = connect(addr, "serverId=legit&role=server").await;
    server.shutdown().await;
}

#[tokio::test]
async fn per_ip_cap_also_bounds_pre_upgrade_sockets() {
    let mut config = test_config();
    config.per_ip_max_connections = 1;
    let (server, addr) = start(config).await;

    // per_ip_max_connections = 1 allows 9 TCP sockets per client (the
    // WebSocket cap plus headroom for sockets still in the HTTP phase).
    let mut held = Vec::new();
    for _ in 0..9 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("GET /health HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut chunk = [0u8; 256];
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(String::from_utf8_lossy(&chunk[..read]).starts_with("HTTP/1.1 200"));
        held.push(stream);
    }

    let mut extra = TcpStream::connect(addr).await.unwrap();
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), extra.read_to_end(&mut response)).await;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 429") && response.contains("Per-IP connection limit"),
        "tenth socket from one client must be refused, got {response:?}"
    );

    // The node itself is nowhere near its TCP cap, and the slot comes back.
    drop(held);
    let mut status = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        status = lenient_http_status(addr, "/health").await;
        if status == Some(200) {
            break;
        }
    }
    assert_eq!(status, Some(200));
    assert!(metric(addr, "paseo_relay_tcp_rejections_total").await >= 1);
    server.shutdown().await;
}

#[tokio::test]
async fn ops_listener_takes_operations_endpoints_off_the_public_port() {
    let mut config = test_config();
    config.ops_port = Some(0);
    let (server, addr) = start(config).await;
    let ops = server.ops_local_addr().expect("operations listener bound");
    assert_ne!(ops, addr);

    assert_eq!(http_get(addr, "/health").await.0, 200);
    for path in ["/ready", "/metrics", "/version"] {
        assert_eq!(http_get(addr, path).await.0, 404, "public {path}");
        assert_eq!(http_get(ops, path).await.0, 200, "operations {path}");
    }
    assert_eq!(http_get(ops, "/health").await.0, 200);

    // WebSockets are a public-listener feature only.
    let _daemon = connect(addr, "serverId=ops&role=server").await;
    let request = upgrade_request(
        ops,
        "/ws?serverId=ops&role=client",
        Some("dGhlIHNhbXBsZSBub25jZQ=="),
    )
    .await;
    assert_eq!(raw_http(ops, &request).await.0, 404);
    let (_, metrics) = http_get(ops, "/metrics").await;
    assert!(metrics.contains("paseo_relay_active_websockets 1"));
    server.shutdown().await;
}

#[tokio::test]
async fn ops_listener_answers_while_the_public_listener_is_full() {
    let mut config = test_config();
    config.max_connections = 1;
    config.max_tcp_connections = 1;
    config.ops_port = Some(0);
    let (server, addr) = start(config).await;
    let ops = server.ops_local_addr().unwrap();

    let _ws = connect(addr, "serverId=full&role=server").await;
    assert_ne!(lenient_http_status(addr, "/health").await, Some(200));

    // Liveness still answers, and readiness reports the truth.
    assert_eq!(http_get(ops, "/health").await.0, 200);
    assert_eq!(http_get(ops, "/ready").await.0, 503);

    // The container healthcheck follows the operations listener, so a full
    // public listener does not get the container restarted.
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_paseo-relay"))
        .arg("healthcheck")
        .env("PASEO_RELAY_PORT", addr.port().to_string())
        .env("PASEO_RELAY_OPS_PORT", ops.port().to_string())
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
async fn idle_timeout_closes_silent_sockets_but_not_ones_receiving_traffic() {
    let mut config = test_config();
    config.ws_idle_timeout = Duration::from_secs(1);
    let (server, addr) = start(config).await;

    let mut silent = connect(addr, "serverId=idle-a&role=client").await;
    let mut daemon = connect(addr, "serverId=idle-b&role=server").await;
    let mut listener = connect(addr, "serverId=idle-b&role=client").await;

    // `listener` never sends anything, but the daemon keeps relaying to it.
    let started = tokio::time::Instant::now();
    let mut received = 0;
    while started.elapsed() < Duration::from_millis(1800) {
        daemon.send(Message::Text("tick".into())).await.unwrap();
        assert_eq!(expect_text(&mut listener).await, "tick");
        received += 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(received >= 6);

    // Meanwhile the socket with no traffic at all was closed.
    assert_eq!(
        expect_close(&mut silent).await,
        (1013, "Idle timeout".to_owned())
    );
    assert_eq!(metric(addr, "paseo_relay_idle_timeouts_total").await, 1);

    // And once the relayed traffic stops, the listener goes idle too.
    drop(daemon);
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match listener.next().await {
                Some(Ok(Message::Close(Some(frame)))) => {
                    return (u16::from(frame.code), frame.reason.to_string())
                }
                Some(Ok(_)) => continue,
                other => panic!("expected idle close, got {other:?}"),
            }
        }
    })
    .await
    .expect("listener must be closed once it is idle");
    assert_eq!(closed, (1013, "Idle timeout".to_owned()));
    server.shutdown().await;
}

#[tokio::test]
async fn idle_sockets_are_kept_when_no_idle_timeout_is_configured() {
    // The documented default: the relay never closes a socket for being idle.
    let (server, addr) = start(test_config()).await;
    let mut client = connect(addr, "serverId=quiet&role=client").await;
    assert!(
        next_within(&mut client, Duration::from_millis(1500))
            .await
            .is_none(),
        "socket must stay open"
    );
    assert_eq!(metric(addr, "paseo_relay_active_websockets").await, 1);
    server.shutdown().await;
}

// --- native TLS -------------------------------------------------------------

struct TlsFiles {
    dir: std::path::PathBuf,
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
}

impl TlsFiles {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "paseo-relay-tls-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            cert: dir.join("cert.pem"),
            key: dir.join("key.pem"),
            dir,
        }
    }

    /// Write a fresh self-signed certificate for `localhost` and return it.
    fn issue(&self) -> rustls::pki_types::CertificateDer<'static> {
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        std::fs::write(&self.cert, issued.cert.pem()).unwrap();
        std::fs::write(&self.key, issued.signing_key.serialize_pem()).unwrap();
        issued.cert.der().clone()
    }
}

impl Drop for TlsFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type TlsClient = WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

/// Open a WebSocket over TLS, trusting only `trusted`.
async fn connect_tls(
    addr: std::net::SocketAddr,
    trusted: &rustls::pki_types::CertificateDer<'static>,
    query: &str,
) -> Result<TlsClient, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(trusted.clone()).map_err(|e| e.to_string())?;
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let stream = connector
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (ws, response) =
        tokio_tungstenite::client_async(format!("ws://localhost/ws?{query}"), stream)
            .await
            .map_err(|e| e.to_string())?;
    assert_eq!(response.status(), 101);
    Ok(ws)
}

#[tokio::test]
async fn native_tls_serves_websockets_and_the_healthcheck() {
    let files = TlsFiles::new("serve");
    let cert = files.issue();
    let mut config = test_config();
    config.tls_cert = Some(files.cert.clone());
    config.tls_key = Some(files.key.clone());
    let (server, addr) = start(config).await;

    let mut daemon = connect_tls(addr, &cert, "serverId=tls&role=server")
        .await
        .unwrap();
    let mut client = connect_tls(addr, &cert, "serverId=tls&role=client")
        .await
        .unwrap();
    client.send(Message::Text("over-tls".into())).await.unwrap();
    assert_eq!(
        daemon.next().await.unwrap().unwrap(),
        Message::Text("over-tls".into())
    );
    daemon
        .send(Message::Binary(vec![9u8; 70_000].into()))
        .await
        .unwrap();
    assert_eq!(
        client.next().await.unwrap().unwrap(),
        Message::Binary(vec![9u8; 70_000].into())
    );

    // Plaintext on a TLS listener is not served.
    assert_ne!(lenient_http_status(addr, "/health").await, Some(200));

    // The self-probe speaks TLS when the relay does.
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_paseo-relay"))
        .arg("healthcheck")
        .env("PASEO_RELAY_PORT", addr.port().to_string())
        .env("PASEO_RELAY_TLS_CERT", &files.cert)
        .env("PASEO_RELAY_TLS_KEY", &files.key)
        .output()
        .await
        .expect("run healthcheck");
    assert!(
        output.status.success(),
        "tls healthcheck failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.shutdown().await;
}

#[tokio::test]
async fn tls_certificate_reloads_without_dropping_sessions() {
    let files = TlsFiles::new("reload");
    let first = files.issue();
    let mut config = test_config();
    config.tls_cert = Some(files.cert.clone());
    config.tls_key = Some(files.key.clone());
    let (server, addr) = start(config).await;

    let mut daemon = connect_tls(addr, &first, "serverId=keep&role=server")
        .await
        .unwrap();

    // Rotate the certificate on disk and reload, as SIGHUP does.
    let second = files.issue();
    assert!(server.reload_tls().unwrap());

    // New handshakes get the new certificate...
    let mut client = connect_tls(addr, &second, "serverId=keep&role=client")
        .await
        .expect("new certificate must be served after reload");
    assert!(
        connect_tls(addr, &first, "serverId=other&role=client")
            .await
            .is_err(),
        "the old certificate must no longer be served"
    );

    // ...and the session opened before the reload is still alive.
    client
        .send(Message::Text("still-here".into()))
        .await
        .unwrap();
    assert_eq!(
        daemon.next().await.unwrap().unwrap(),
        Message::Text("still-here".into())
    );

    // A broken file is refused and the working certificate stays in use.
    std::fs::write(&files.cert, "not a certificate").unwrap();
    assert!(server.reload_tls().is_err());
    assert!(connect_tls(addr, &second, "serverId=after&role=client")
        .await
        .is_ok());
    server.shutdown().await;
}

#[tokio::test]
async fn reload_tls_reports_when_tls_is_not_configured() {
    let (server, _addr) = start(test_config()).await;
    assert!(!server.reload_tls().unwrap());
    server.shutdown().await;
}

#[tokio::test]
async fn v2_session_caps_distinct_connections_and_deduplicates_watchdog() {
    let mut config = test_config();
    config.max_connections_per_session = 3;
    config.control_nudge_after = Duration::from_millis(250);
    config.control_unresponsive_after = Duration::from_millis(550);
    let (server, addr) = start(config).await;

    let mut control = connect(addr, "serverId=cap-s&role=server&v=2&mode=control").await;
    let initial: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
    assert_eq!(initial["type"], "sync");
    assert_eq!(initial["connectionIds"], serde_json::json!([]));

    let mut clients = Vec::new();
    for idx in 1..=3 {
        let client = connect(
            addr,
            &format!("serverId=cap-s&role=client&v=2&connectionId=c{idx}"),
        )
        .await;
        let event: Value = serde_json::from_str(&expect_text(&mut control).await).unwrap();
        assert_eq!(event["type"], "connected");
        clients.push(client);
    }

    // A 4th distinct connectionId exceeds max_connections_per_session.
    let mut extra = connect(addr, "serverId=cap-s&role=client&v=2&connectionId=c4").await;
    assert_eq!(
        expect_close(&mut extra).await,
        (1013, "Relay session connection capacity".to_owned())
    );

    // Without data sockets attaching, the single session-level watchdog must
    // emit only one nudge `sync` (not one per waiting client), then close the
    // unresponsive control socket with 1011.
    let mut sync_nudges = 0usize;
    let mut closed_reason = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), control.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                if value["type"] == "sync" {
                    sync_nudges += 1;
                    assert_eq!(
                        value["connectionIds"],
                        serde_json::json!(["c1", "c2", "c3"])
                    );
                }
            }
            Ok(Some(Ok(Message::Close(Some(frame))))) => {
                closed_reason = Some((u16::from(frame.code), frame.reason.to_string()));
                break;
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => break,
        }
    }
    assert_eq!(
        sync_nudges, 1,
        "watchdog must emit a single deduplicated sync nudge"
    );
    assert_eq!(
        closed_reason,
        Some((1011, "Control unresponsive".to_owned()))
    );

    drop(clients);
    server.shutdown().await;
}

#[tokio::test]
async fn replacing_v1_source_keeps_writer_budget_and_enforces_delivery_deadline() {
    let mut config = test_config();
    config.max_message_bytes = 2 * 1024 * 1024;
    config.ingress_budget_bytes = 128 * 1024 * 1024;
    config.delivery_timeout = Duration::from_millis(1200);
    let (server, addr) = start(config).await;

    // Connect a raw destination that never reads after upgrade, so frames back
    // up inside its outbound writer queue.
    let mut stalled_dest = raw_ws(addr, "serverId=crit2&role=server").await;
    let source_a = connect(addr, "serverId=crit2&role=client").await;
    let (mut source_a_write, mut source_a_read) = source_a.split();

    // Push 512 KiB messages from a background task until loopback buffers fill
    // and frames queue inside the destination's writer (well before the 1.2 s
    // delivery deadline).
    let sender = tokio::spawn(async move {
        let chunk = vec![0xABu8; 512 * 1024];
        for _ in 0..32 {
            if source_a_write
                .send(Message::Binary(chunk.clone().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let wait_until = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < wait_until {
        if metric(addr, "paseo_relay_ingress_reserved_bytes").await >= 2 * 1024 * 1024 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        metric(addr, "paseo_relay_ingress_reserved_bytes").await >= 2 * 1024 * 1024,
        "frames must be queued in the stalled destination writer before replacement"
    );

    // Replace the v1 source while frames are still queued for `stalled_dest`.
    let _source_b = connect(addr, "serverId=crit2&role=client").await;
    let close = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match source_a_read.next().await {
                Some(Ok(Message::Close(Some(frame)))) => {
                    return (u16::from(frame.code), frame.reason.to_string());
                }
                Some(Ok(_)) => continue,
                other => panic!("expected replacement close, got {other:?}"),
            }
        }
    })
    .await
    .expect("old source must receive replacement close");
    assert_eq!(close.0, 1008);
    sender.abort();
    drop(source_a_read);

    // Give the replaced source task a moment to finish detaching, then confirm
    // the queued frames in `stalled_dest`'s writer still hold ingress budget.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let reserved = metric(addr, "paseo_relay_ingress_reserved_bytes").await;
    assert!(
        reserved > 0,
        "queued frames must retain their ingress budget after source replacement"
    );

    // Once the absolute delivery deadline expires, the stalled destination is
    // shed independently and all queued budgets return to 0.
    wait_for_metric(addr, "paseo_relay_ingress_reserved_bytes", 0).await;
    assert!(metric(addr, "paseo_relay_slow_consumer_disconnects_total").await >= 1);

    let _ = stalled_dest.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn empty_and_one_byte_continuations_hit_stall_timeout_and_never_charge_small_reserve() {
    let mut config = test_config();
    config.read_stall_timeout = Duration::from_millis(400);
    let (server, addr) = start(config).await;

    // 1) 64 KiB opening fragment (fin=0) must use bulk, not small reserve, and
    //    periodic 0-byte continuation frames must not defeat read_stall_timeout.
    let zero_sock = park_unfinished_fragment(addr, "zero-cont", 64 * 1024).await;
    assert_eq!(
        metric(addr, "paseo_relay_ingress_small_reserved_bytes").await,
        0,
        "non-final fragment must not charge the small reserve"
    );
    let (mut zero_read, mut zero_write) = zero_sock.into_split();
    let zero_sender = tokio::spawn(async move {
        let empty_cont = frame_header(false, 0x0, 0);
        loop {
            tokio::time::sleep(Duration::from_millis(120)).await;
            if zero_write.write_all(&empty_cont).await.is_err() {
                break;
            }
        }
    });
    let mut zero_buf = Vec::new();
    let _ =
        tokio::time::timeout(Duration::from_secs(3), zero_read.read_to_end(&mut zero_buf)).await;
    zero_sender.abort();
    assert_eq!(
        parse_close(&zero_buf),
        Some((1013, "Relay read timeout".to_owned()))
    );

    // 2) 1-byte continuation frames must also fail to defeat read_stall_timeout
    //    because progress is measured in full INGRESS_CHUNK_BYTES (8 KiB) chunks.
    let one_sock = park_unfinished_fragment(addr, "one-cont", 16).await;
    let (mut one_read, mut one_write) = one_sock.into_split();
    let one_sender = tokio::spawn(async move {
        let mut one_cont = frame_header(false, 0x0, 1);
        one_cont.push(0x42);
        loop {
            tokio::time::sleep(Duration::from_millis(120)).await;
            if one_write.write_all(&one_cont).await.is_err() {
                break;
            }
        }
    });
    let mut one_buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), one_read.read_to_end(&mut one_buf)).await;
    one_sender.abort();
    assert_eq!(
        parse_close(&one_buf),
        Some((1013, "Relay read timeout".to_owned()))
    );

    server.shutdown().await;
}

#[tokio::test]
async fn stalled_http_response_times_out_and_releases_tcp_slot() {
    let mut config = test_config();
    config.http_header_timeout = Duration::from_millis(400);
    config.max_tcp_connections = 1;
    config.max_connections = 1;
    config.per_ip_max_connections = 0;
    let (server, addr) = start(config).await;

    // Hold the single TCP slot by pipelining hundreds of /metrics requests
    // without reading any response bytes.
    let mut stalled = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET /metrics HTTP/1.1\r\nHost: {addr}\r\n\r\n");
    let pipeline = req.repeat(600);
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        stalled.write_all(pipeline.as_bytes()),
    )
    .await;

    // While the socket is pinned, a second peer is turned away at the TCP cap.
    assert_eq!(lenient_http_status(addr, "/health").await, Some(503));

    // After http_header_timeout elapses, the stalled HTTP writer must time out
    // and release its TCP slot so /health succeeds again.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut recovered = false;
    while tokio::time::Instant::now() < deadline {
        if lenient_http_status(addr, "/health").await == Some(200) {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        recovered,
        "stalled HTTP response writer must time out and release its TCP slot"
    );

    drop(stalled);
    server.shutdown().await;
}

#[tokio::test]
async fn shared_port_drain_keeps_ops_reachable_and_rejects_ws() {
    let mut config = test_config();
    config.shutdown_grace = Duration::from_millis(600);
    let (server, addr) = start(config).await;

    let shutdown = tokio::spawn(async move { server.shutdown().await });
    tokio::time::sleep(Duration::from_millis(60)).await;

    let (health_status, _) = http_get(addr, "/health").await;
    assert_eq!(health_status, 200);

    let (ready_status, _) = http_get(addr, "/ready").await;
    assert_eq!(ready_status, 503);

    let (metrics_status, metrics_body) = http_get(addr, "/metrics").await;
    assert_eq!(metrics_status, 200);
    assert!(metrics_body.contains("paseo_relay_draining 1"));
    assert!(metrics_body.contains("paseo_relay_ready 0"));

    let req = upgrade_request(
        addr,
        "/ws?serverId=during-drain&role=client",
        Some("dGhlIHNhbXBsZSBub25jZQ=="),
    )
    .await;
    let (ws_status, _) = raw_http(addr, &req).await;
    assert_eq!(ws_status, 503);

    shutdown.await.unwrap();
}

#[tokio::test]
async fn duplicate_origin_and_http10_ws_upgrade_are_rejected() {
    let mut config = test_config();
    config.allowed_origins = vec!["https://app.example".to_owned()];
    let (server, addr) = start(config).await;

    let dup_origin = format!(
        "GET /ws?serverId=orig&role=client HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Origin: https://app.example\r\n\
         Origin: https://evil.example\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    let (status, _) = raw_http(addr, &dup_origin).await;
    assert_eq!(status, 403);

    let http10 = format!(
        "GET /ws?serverId=v10&role=client HTTP/1.0\r\n\
         Host: {addr}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    let (status, _) = raw_http(addr, &http10).await;
    assert_eq!(status, 400);

    server.shutdown().await;
}

#[tokio::test]
async fn healthcheck_ignores_empty_tls_vars_and_follows_bound_host() {
    let (server, addr) = start(test_config()).await;

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_paseo-relay"))
        .arg("healthcheck")
        .env_remove("PASEO_RELAY_HEALTHCHECK_HOST")
        .env_remove("PASEO_RELAY_OPS_PORT")
        .env("PASEO_RELAY_HOST", addr.ip().to_string())
        .env("PASEO_RELAY_PORT", addr.port().to_string())
        .env("PASEO_RELAY_TLS_CERT", "")
        .env("PASEO_RELAY_TLS_KEY", "")
        .output()
        .await
        .expect("run healthcheck");
    assert!(
        output.status.success(),
        "healthcheck with empty TLS vars failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.shutdown().await;
}

#[tokio::test]
async fn truncated_control_or_continuation_frame_still_hits_stall_timeout() {
    let mut config = test_config();
    config.read_stall_timeout = Duration::from_millis(400);
    config.frame_read_timeout = Duration::from_secs(30);
    config.message_read_timeout = Duration::from_secs(60);
    let (server, addr) = start(config).await;

    // 1) Non-final fragment followed by a Ping header whose 1-byte payload
    //    never arrives must close at read_stall_timeout (400 ms), not
    //    frame_read_timeout (30 s).
    let mut ping_stall = park_unfinished_fragment(addr, "ping-stall", 16).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let truncated_ping = frame_header(true, 0x9, 1);
    ping_stall.write_all(&truncated_ping).await.unwrap();
    let started = tokio::time::Instant::now();
    assert_eq!(
        raw_close(&mut ping_stall, Duration::from_secs(2)).await,
        Some((1013, "Relay read timeout".to_owned()))
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "truncated Ping must not extend stall timeout to frame_read_timeout"
    );

    // 2) Non-final fragment followed near the end of read_stall_timeout by a
    //    truncated 1-byte continuation header must not grant a fresh stall
    //    window on top of the elapsed time.
    let mut cont_stall = park_unfinished_fragment(addr, "cont-stall", 16).await;
    let open_at = tokio::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let truncated_cont = frame_header(false, 0x0, 1);
    cont_stall.write_all(&truncated_cont).await.unwrap();
    assert_eq!(
        raw_close(&mut cont_stall, Duration::from_secs(2)).await,
        Some((1013, "Relay read timeout".to_owned()))
    );
    assert!(
        open_at.elapsed() < Duration::from_millis(600),
        "truncated continuation header must not reset the stall deadline (elapsed {:?})",
        open_at.elapsed()
    );

    server.shutdown().await;
}
