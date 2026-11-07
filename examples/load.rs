//! Minimal load generator for a running relay.
//!
//! Opens `pairs` v1 server/client WebSocket pairs, then performs `frames`
//! request/response round trips per pair with `size`-byte text messages.
//! Reports aggregate round-trip throughput, p50/p99 latency, and the slowest
//! pair. This is a loopback sanity benchmark, not a rigorous network benchmark.
//!
//! Usage:
//!   cargo run --release --example load -- --addr 127.0.0.1:4000 \
//!     --pairs 100 --frames 500 --size 1024

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Args {
    addr: String,
    pairs: usize,
    frames: usize,
    size: usize,
}

fn parse_args() -> Args {
    let mut args = Args {
        addr: "127.0.0.1:4000".to_owned(),
        pairs: 100,
        frames: 500,
        size: 1024,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter.next().unwrap_or_else(|| {
            eprintln!("missing value for {flag}");
            std::process::exit(2);
        });
        match flag.as_str() {
            "--addr" => args.addr = value,
            "--pairs" => args.pairs = value.parse().expect("--pairs must be an integer"),
            "--frames" => args.frames = value.parse().expect("--frames must be an integer"),
            "--size" => args.size = value.parse().expect("--size must be an integer"),
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    args
}

async fn connect(addr: &str, query: &str) -> Client {
    let (client, _) = connect_async(format!("ws://{addr}/ws?{query}"))
        .await
        .expect("websocket connect");
    client
}

async fn round_trip(client: &mut Client, server: &mut Client, payload: &str) -> Duration {
    let started = Instant::now();
    client
        .send(Message::Text(payload.into()))
        .await
        .expect("client send");
    match server
        .next()
        .await
        .expect("server stream")
        .expect("server frame")
    {
        Message::Text(text) => assert_eq!(text.as_str(), payload),
        other => panic!("unexpected relayed message: {other:?}"),
    }
    server
        .send(Message::Text(payload.into()))
        .await
        .expect("server send");
    match client
        .next()
        .await
        .expect("client stream")
        .expect("client frame")
    {
        Message::Text(text) => assert_eq!(text.as_str(), payload),
        other => panic!("unexpected relayed message: {other:?}"),
    }
    started.elapsed()
}

fn percentile(sorted: &[Duration], percentile: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() as f64 - 1.0) * percentile).round() as usize;
    sorted[index]
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let payload = "x".repeat(args.size);
    let run = format!("load-{}", std::process::id());

    let mut sockets = Vec::with_capacity(args.pairs);
    for index in 0..args.pairs {
        let server_id = format!("{run}-{index}");
        let server = connect(&args.addr, &format!("serverId={server_id}&role=server")).await;
        let client = connect(&args.addr, &format!("serverId={server_id}&role=client")).await;
        sockets.push((server, client));
    }

    println!(
        "connected {} pairs to {}; starting {} round trips per pair ({} B messages)",
        args.pairs, args.addr, args.frames, args.size
    );

    let started = Instant::now();
    let mut tasks = Vec::with_capacity(sockets.len());
    for (mut server, mut client) in sockets {
        let payload = payload.clone();
        tasks.push(tokio::spawn(async move {
            // Warm up both directions before measuring.
            let _ = round_trip(&mut client, &mut server, &payload).await;
            let mut latencies = Vec::with_capacity(args.frames);
            for _ in 0..args.frames {
                latencies.push(round_trip(&mut client, &mut server, &payload).await);
            }
            latencies
        }));
    }

    let mut latencies = Vec::with_capacity(args.pairs * args.frames);
    for task in tasks {
        latencies.extend(task.await.expect("load task"));
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    let frames = latencies.len();
    let per_second = frames as f64 / elapsed.as_secs_f64();
    let mib = (frames * args.size * 2) as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64();
    println!(
        "{} round trips in {:.3}s: {:.0} msg/s, {:.1} MiB/s (both directions)",
        frames,
        elapsed.as_secs_f64(),
        per_second,
        mib
    );
    println!(
        "latency: p50 {:?}, p99 {:?}, max {:?}",
        percentile(&latencies, 0.50),
        percentile(&latencies, 0.99),
        latencies.last().copied().unwrap_or_default()
    );
    println!("note: loopback client, one host, text frames; not a WAN benchmark");
}
