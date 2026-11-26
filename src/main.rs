#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use paseo_relay::config::{open_file_limit, unknown_vars};
use paseo_relay::{serve, Config, ServerHandle, BUILD_REVISION, VERSION};
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
            // Not errors: the relay starts either way. They are printed so a
            // deployment check catches them before traffic does.
            for warning in startup_warnings(&config) {
                println!("warning: {warning}");
            }
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
    // Config::from_env validates the filter; propagate instead of silently
    // falling back so the documented startup contract holds.
    let filter = EnvFilter::try_new(&config.log_filter)
        .context("PASEO_RELAY_LOG must be a valid tracing EnvFilter directive")?;
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if config.log_json {
        builder.json().init();
    } else {
        builder.init();
    }

    tracing::info!(config = %config, "starting paseo relay");
    for warning in startup_warnings(&config) {
        tracing::warn!("{warning}");
    }
    let server = serve(config).await?;
    tracing::info!(addr = %server.local_addr(), "ready");

    wait_for_shutdown_signal(&server).await;
    tracing::info!("shutdown signal received");
    server.shutdown().await;
    Ok(())
}

/// Deployment mistakes that do not stop the relay but will hurt it later.
fn startup_warnings(config: &Config) -> Vec<String> {
    let mut warnings: Vec<String> =
        unknown_vars(std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()))
            .into_iter()
            .map(|name| {
                format!("{name} is set but is not a relay setting (misspelled?); it is ignored")
            })
            .collect();
    warnings.extend(config.open_file_limit_warning(open_file_limit()));
    warnings.extend(config.per_ip_hint());
    warnings
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
           PASEO_RELAY_OPS_PORT        optional private port for /ready /metrics /version\n\
           PASEO_RELAY_DRAIN=false     start unready and reject new sessions\n\
           PASEO_RELAY_TLS_CERT/KEY    optional PEM certificate/key for native TLS\n\
           PASEO_RELAY_LOG=info        tracing EnvFilter directive\n\
           PASEO_RELAY_LOG_FORMAT=text text or json structured logs\n\n\
         Endpoints: GET /ws  GET /health  GET /ready  GET /metrics  GET /version\n\
         Send SIGTERM or SIGINT for a graceful drain, SIGHUP to reload the TLS\n\
         certificate and key."
    );
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn env_port(name: &str) -> Option<u16> {
    env_non_empty(name)
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|&port| port > 0)
}

fn probe_host_for_bind(raw: Option<&str>) -> String {
    match raw.and_then(|value| value.trim().parse::<IpAddr>().ok()) {
        Some(ip) if !ip.is_unspecified() => ip.to_string(),
        Some(IpAddr::V6(_)) => "::1".to_owned(),
        _ => "127.0.0.1".to_owned(),
    }
}

fn format_host_header(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Container-friendly self-probe for the scratch image: a raw HTTP GET /health.
///
/// Uses `PASEO_RELAY_HEALTHCHECK_HOST` (defaulting to the bound listener
/// address, or `127.0.0.1`/`::1` for wildcard binds) and `PASEO_RELAY_PORT`
/// (default `4000`). Exit code 0 means the relay answered 200; anything else
/// means unhealthy. When `PASEO_RELAY_TLS_CERT` and `PASEO_RELAY_TLS_KEY` are
/// non-empty, the probe speaks TLS with certificate signature verification
/// (hostname/trust-chain checking is skipped because this only ever connects
/// to the local relay).
///
/// With `PASEO_RELAY_OPS_PORT` set the probe uses that listener instead (and
/// follows `PASEO_RELAY_OPS_HOST` when `PASEO_RELAY_HEALTHCHECK_HOST` is unset).
/// It is plaintext, and it has its own connection allowance, so the container
/// is not reported unhealthy merely because the public listener is full.
fn healthcheck() -> Result<()> {
    let ops_port = env_port("PASEO_RELAY_OPS_PORT");
    let port = ops_port
        .or_else(|| env_port("PASEO_RELAY_PORT"))
        .unwrap_or(4000);
    let host = match env_non_empty("PASEO_RELAY_HEALTHCHECK_HOST") {
        Some(value) => value
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(&value)
            .to_owned(),
        None => {
            let bind_var = if ops_port.is_some() {
                "PASEO_RELAY_OPS_HOST"
            } else {
                "PASEO_RELAY_HOST"
            };
            let bind = env_non_empty(bind_var);
            probe_host_for_bind(bind.as_deref())
        }
    };
    let host_header = format_host_header(&host, port);
    let socket_addr = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve {host_header}"))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(2))
        .map_err(|error| anyhow!("healthcheck connect {host_header}: {error}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let tls = ops_port.is_none()
        && env_non_empty("PASEO_RELAY_TLS_CERT").is_some()
        && env_non_empty("PASEO_RELAY_TLS_KEY").is_some();
    let request =
        format!("GET /health HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");

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

/// Wait for SIGTERM/SIGINT. SIGHUP reloads the TLS certificate and keeps
/// waiting.
async fn wait_for_shutdown_signal(server: &ServerHandle) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut terminate), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => return,
                _ = terminate.recv() => return,
                _ = hangup.recv() => match server.reload_tls_async().await {
                    Ok(true) => tracing::info!("SIGHUP: tls certificate reloaded"),
                    Ok(false) => tracing::info!("SIGHUP: native TLS is not configured; nothing to reload"),
                    Err(error) => tracing::warn!(
                        error = %format!("{error:#}"),
                        "SIGHUP: tls reload failed; keeping the previous certificate"
                    ),
                },
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = server;
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_host_follows_non_wildcard_bind_and_loopback_for_wildcard() {
        assert_eq!(probe_host_for_bind(None), "127.0.0.1");
        assert_eq!(probe_host_for_bind(Some("0.0.0.0")), "127.0.0.1");
        assert_eq!(probe_host_for_bind(Some("::")), "::1");
        assert_eq!(probe_host_for_bind(Some("10.0.0.5")), "10.0.0.5");
        assert_eq!(probe_host_for_bind(Some("fd00::1")), "fd00::1");
    }

    #[test]
    fn host_header_brackets_ipv6_literals() {
        assert_eq!(format_host_header("127.0.0.1", 4000), "127.0.0.1:4000");
        assert_eq!(format_host_header("::1", 4001), "[::1]:4001");
        assert_eq!(format_host_header("localhost", 4000), "localhost:4000");
    }
}
