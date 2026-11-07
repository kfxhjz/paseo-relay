use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use paseo_relay::{serve, Config, BUILD_REVISION, VERSION};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConnection, DigitallySignedStruct, SignatureScheme};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--help" | "-h") => {
            print_help();
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("paseo-relay {VERSION} ({BUILD_REVISION})");
            return Ok(());
        }
        Some("--check-config") => {
            let config = Config::from_env()?;
            println!("configuration ok: {config}");
            return Ok(());
        }
        Some("--dump-config") => {
            let config = Config::from_env()?;
            println!("{config:#?}");
            return Ok(());
        }
        Some("healthcheck") => return healthcheck(),
        Some(unknown) => {
            eprintln!(
                "paseo-relay: unknown argument {unknown:?}; configuration is environment-only (try --help)"
            );
            std::process::exit(2);
        }
        None => {}
    }

    let config = Config::from_env()?;
    let filter = EnvFilter::try_new(&config.log_filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if config.log_json {
        builder.json().init();
    } else {
        builder.init();
    }

    tracing::info!(config = %config, "starting paseo relay");
    let server = serve(config).await?;
    tracing::info!(addr = %server.local_addr(), "ready");

    wait_for_shutdown_signal().await;
    tracing::info!("shutdown signal received");
    server.shutdown().await;
    Ok(())
}

fn print_help() {
    println!(
        "paseo-relay {VERSION} - single-node, wire-compatible WebSocket relay for Paseo\n\n\
         Usage:\n\
           paseo-relay [--help|--version|--check-config|--dump-config]\n\
           paseo-relay healthcheck\n\n\
         The relay is configured through environment variables; see README.md\n\
         for the full runtime contract. CLI flags only inspect the configuration.\n\n\
         Common settings:\n\
           PASEO_RELAY_HOST=127.0.0.1   listener IP\n\
           PASEO_RELAY_PORT=4000       listener port (HTTP, WebSocket, ops)\n\
           PASEO_RELAY_DRAIN=false     start unready and reject new sessions\n\
           PASEO_RELAY_TLS_CERT/KEY    optional PEM certificate/key for native TLS\n\
           PASEO_RELAY_LOG=info        tracing EnvFilter directive\n\
           PASEO_RELAY_LOG_FORMAT=text text or json structured logs\n\n\
         Endpoints: GET /ws  GET /health  GET /ready  GET /metrics  GET /version\n\
         Send SIGTERM or SIGINT for a graceful drain."
    );
}

/// Container-friendly self-probe for the scratch image: a raw HTTP GET /health.
///
/// Uses `PASEO_RELAY_HEALTHCHECK_HOST` (default `127.0.0.1`) and
/// `PASEO_RELAY_PORT` (default `4000`). Exit code 0 means the relay answered
/// 200; anything else means unhealthy. When `PASEO_RELAY_TLS_CERT` and
/// `PASEO_RELAY_TLS_KEY` are set, the probe speaks TLS with certificate
/// signature verification (hostname/trust-chain checking is skipped because
/// this only ever connects to the local relay).
fn healthcheck() -> Result<()> {
    let host =
        std::env::var("PASEO_RELAY_HEALTHCHECK_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
    let port: u16 = std::env::var("PASEO_RELAY_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4000);
    let address = format!("{host}:{port}");
    let socket_addr = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve {address}"))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(2))
        .map_err(|error| anyhow!("healthcheck connect {address}: {error}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let tls = std::env::var("PASEO_RELAY_TLS_CERT").is_ok()
        && std::env::var("PASEO_RELAY_TLS_KEY").is_ok();
    let request = format!("GET /health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");

    if tls {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .ok_or_else(|| anyhow!("no rustls crypto provider"))?;
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification { provider }))
            .with_no_client_auth();
        let server_name = ServerName::try_from("localhost")
            .map_err(|error| anyhow!("invalid TLS healthcheck server name: {error}"))?;
        let connection = ClientConnection::new(Arc::new(config), server_name)
            .map_err(|error| anyhow!("TLS healthcheck setup: {error}"))?;
        let mut tls_stream = rustls::StreamOwned::new(connection, stream);
        tls_stream.write_all(request.as_bytes())?;
        read_health_response(&mut tls_stream)
    } else {
        stream.write_all(request.as_bytes())?;
        read_health_response(&mut stream)
    }
}

fn read_health_response(mut stream: impl Read) -> Result<()> {
    // A single read() may return only part of the status line, especially over
    // TLS. Read until the first line is available (or the probe buffer fills).
    let mut response = Vec::with_capacity(512);
    let mut chunk = [0u8; 256];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if response.windows(2).any(|window| window == b"\r\n") || response.len() >= 512 {
            break;
        }
    }
    let first_line = String::from_utf8_lossy(&response);
    let status_line = first_line.lines().next().unwrap_or("empty response");
    if status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        bail!("healthcheck failed: {status_line}")
    }
}

/// Healthchecks connect only to loopback. The certificate signature is still
/// verified; the trust chain and hostname are intentionally not checked because
/// a self-signed certificate is a valid local deployment.
#[derive(Debug)]
struct NoCertificateVerification {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
