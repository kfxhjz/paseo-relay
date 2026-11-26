#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use paseo_relay::config::{open_file_limit, unknown_vars};
use paseo_relay::{serve, Config, ServerHandle, BUILD_REVISION, VERSION};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
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
    tracing::info!("shutdown complete");
    // A background task may still be parked in spawn_blocking on a hung
    // certificate mount. Waiting for the Tokio runtime to drop would wait for
    // that blocking thread forever, so leave the process deliberately after
    // the graceful drain instead of requiring SIGKILL.
    std::process::exit(0);
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
    warnings.extend(config.per_connection_limit_warning());
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
/// means unhealthy.
///
/// With native TLS configured (`PASEO_RELAY_TLS_CERT`/`PASEO_RELAY_TLS_KEY`
/// non-empty) the probe speaks TLS and pins the certificate read from
/// `PASEO_RELAY_TLS_CERT`: only the relay's own leaf certificate is accepted,
/// so another local service (or a path MITM) cannot answer `200` with a
/// different certificate. If native TLS is configured but the certificate file
/// cannot be read, the probe only trusts a loopback target; a non-loopback
/// target fails closed. A plaintext listener has no certificate to pin:
/// non-loopback probes are allowed but unauthenticated.
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

    // The self-probe must not believe an arbitrary local service that answers
    // 200 on the listener address.
    let tls = ops_port.is_none()
        && env_non_empty("PASEO_RELAY_TLS_CERT").is_some()
        && env_non_empty("PASEO_RELAY_TLS_KEY").is_some();
    let pin = if tls {
        env_non_empty("PASEO_RELAY_TLS_CERT")
            .and_then(|path| certificate_sha256_pin(Path::new(&path)).ok())
    } else {
        None
    };
    ensure_probe_target_allowed(socket_addr.ip(), &host_header, tls, pin.is_some())?;

    let mut stream = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(2))
        .map_err(|error| anyhow!("healthcheck connect {host_header}: {error}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let request =
        format!("GET /health HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");

    if tls {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .ok_or_else(|| anyhow!("no rustls crypto provider"))?;
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertificateVerification {
                provider,
                pin,
            }))
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

/// Refuse a probe target that cannot be authenticated.
///
/// `has_pin` means the relay's certificate was read and its DER SHA-256 will be
/// enforced on the handshake. Otherwise a native TLS target is refused unless
/// it is loopback; a plaintext target is allowed but unauthenticated, because
/// there is no certificate to pin.
fn ensure_probe_target_allowed(
    ip: IpAddr,
    host_header: &str,
    tls: bool,
    has_pin: bool,
) -> Result<()> {
    if has_pin || ip.is_loopback() || !tls {
        return Ok(());
    }
    bail!(
        "healthcheck target {host_header} is not loopback; native TLS is configured but \
         PASEO_RELAY_TLS_CERT could not be read to pin the relay certificate. Fix the certificate \
         path or probe a loopback address"
    )
}

/// SHA-256 of the first (leaf) certificate in `PASEO_RELAY_TLS_CERT`'s PEM file.
fn certificate_sha256_pin(path: &Path) -> Result<[u8; 32]> {
    let certs = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("opening TLS certificate {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parsing TLS certificate {}", path.display()))?;
    let leaf = certs
        .first()
        .ok_or_else(|| anyhow!("TLS certificate {} contains no certificate", path.display()))?;
    Ok(sha256(leaf.as_ref()))
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

/// SHA-256 of `data` (FIPS 180-4).
///
/// The healthcheck only needs a 32-byte certificate fingerprint; implementing
/// the digest here avoids adding a dependency to the static binary for one
/// hash. The tests check it against NIST vectors.
// Random access into the message schedule is clearer here than an iterator
// rewrite, which would need split borrows for no readability gain.
#[allow(clippy::needless_range_loop)]
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Padding: 0x80, zeros, then the message length in bits (big endian).
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = Vec::with_capacity(data.len() + 72);
    message.extend_from_slice(data);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.as_chunks::<64>().0 {
        let mut schedule = [0u32; 64];
        for (word, bytes) in schedule.iter_mut().zip(chunk.as_chunks::<4>().0) {
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for i in 16..64 {
            let s0 = schedule[i - 15].rotate_right(7)
                ^ schedule[i - 15].rotate_right(18)
                ^ (schedule[i - 15] >> 3);
            let s1 = schedule[i - 2].rotate_right(17)
                ^ schedule[i - 2].rotate_right(19)
                ^ (schedule[i - 2] >> 10);
            schedule[i] = schedule[i - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for (&k, &w) in K.iter().zip(schedule.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(w);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut digest = [0u8; 32];
    for (word, bytes) in state.iter().zip(digest.as_chunks_mut::<4>().0) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Healthcheck peer verification that pins the relay's own certificate.
///
/// The probe runs inside the same container/host, and native TLS may use a
/// self-signed certificate, so the trust chain and hostname are not checked.
/// Instead the relay certificate is read from `PASEO_RELAY_TLS_CERT` and its
/// DER SHA-256 must equal the leaf the peer presents. Signature verification
/// still runs. When no pin is available `healthcheck` only probes loopback (see
/// [`ensure_probe_target_allowed`]), so this verifier never accepts an
/// arbitrary certificate from a routable address.
#[derive(Debug)]
struct PinnedCertificateVerification {
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// SHA-256 of the expected leaf certificate; `None` only for loopback.
    pin: Option<[u8; 32]>,
}

impl ServerCertVerifier for PinnedCertificateVerification {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if let Some(pin) = self.pin {
            if sha256(end_entity.as_ref()) != pin {
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure,
                ));
            }
        }
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

/// Wait for SIGTERM/SIGINT. SIGHUP starts a background TLS certificate reload
/// and keeps waiting, so a hung certificate mount cannot block shutdown.
async fn wait_for_shutdown_signal(server: &ServerHandle) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        use tokio::sync::mpsc;
        let (Ok(mut terminate), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        // SIGHUP starts a reload on the blocking pool. A signal that arrives
        // while one is running schedules exactly one more reload after it
        // finishes, so a certificate replaced during the first read is not
        // lost. If the in-flight reload is stuck on a hung mount, the queued
        // rerun cannot start; the signal loop itself still never awaits the
        // reload, so SIGTERM remains responsive.
        let (reload_done, mut reload_done_rx) = mpsc::channel::<()>(1);
        let spawn_reload = |done: mpsc::Sender<()>| -> bool {
            let Some(task) = server.spawn_tls_reload() else {
                return false;
            };
            tokio::spawn(async move {
                match task.await {
                    Ok(Ok(true)) => tracing::info!("SIGHUP: tls certificate reloaded"),
                    Ok(Ok(false)) => {
                        tracing::info!("SIGHUP: native TLS is not configured; nothing to reload")
                    }
                    Ok(Err(error)) => tracing::warn!(
                        error = %format!("{error:#}"),
                        "SIGHUP: tls reload failed; keeping the previous certificate"
                    ),
                    Err(error) => tracing::warn!(
                        error = %error,
                        "SIGHUP: tls reload task failed; keeping the previous certificate"
                    ),
                }
                let _ = done.send(()).await;
            });
            true
        };
        let mut reloading = false;
        let mut reload_pending = false;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => return,
                _ = terminate.recv() => return,
                _ = hangup.recv() => {
                    if reloading {
                        reload_pending = true;
                        tracing::info!(
                            "SIGHUP: tls reload in progress; scheduling one more reload"
                        );
                    } else if spawn_reload(reload_done.clone()) {
                        reloading = true;
                    } else {
                        tracing::info!(
                            "SIGHUP: native TLS is not configured; nothing to reload"
                        );
                    }
                }
                Some(()) = reload_done_rx.recv() => {
                    reloading = false;
                    if reload_pending {
                        reload_pending = false;
                        if spawn_reload(reload_done.clone()) {
                            reloading = true;
                        } else {
                            tracing::info!(
                                "SIGHUP: native TLS is not configured; nothing to reload"
                            );
                        }
                    }
                }
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

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // The NIST long vector also exercises multi-block padding.
        assert_eq!(
            hex(&sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn certificate_pin_hashes_the_leaf_der() {
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let path =
            std::env::temp_dir().join(format!("paseo-healthcheck-pin-{}.pem", std::process::id()));
        std::fs::write(&path, issued.cert.pem()).unwrap();
        let pin = certificate_sha256_pin(&path).unwrap();
        assert_eq!(pin, sha256(issued.cert.der().as_ref()));
        std::fs::remove_file(&path).unwrap();
        assert!(certificate_sha256_pin(&path).is_err());
    }

    #[test]
    fn verifier_accepts_only_the_pinned_certificate() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .unwrap();
        let expected = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let other = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let verifier = PinnedCertificateVerification {
            provider,
            pin: Some(sha256(expected.cert.der().as_ref())),
        };
        let name = ServerName::try_from("localhost").unwrap();
        let now = UnixTime::now();
        assert!(verifier
            .verify_server_cert(expected.cert.der(), &[], &name, &[], now)
            .is_ok());
        assert_eq!(
            verifier
                .verify_server_cert(other.cert.der(), &[], &name, &[], now)
                .unwrap_err(),
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure
            )
        );
    }

    #[test]
    fn non_loopback_probe_requires_a_certificate_pin() {
        let ipv4_loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let ipv6_loopback: IpAddr = "::1".parse().unwrap();
        let routable: IpAddr = "10.0.0.5".parse().unwrap();

        // Loopback is trusted without a pin (plaintext or TLS).
        assert!(ensure_probe_target_allowed(ipv4_loopback, "127.0.0.1:4000", false, false).is_ok());
        assert!(ensure_probe_target_allowed(ipv6_loopback, "[::1]:4000", true, false).is_ok());
        // A plaintext target cannot be authenticated at all; it stays allowed
        // so a relay bound to a concrete non-loopback address keeps a probe.
        assert!(ensure_probe_target_allowed(routable, "10.0.0.5:4000", false, false).is_ok());
        // Native TLS on a routable target requires a pinned certificate.
        assert!(ensure_probe_target_allowed(routable, "10.0.0.5:4000", true, true).is_ok());
        let unreadable =
            ensure_probe_target_allowed(routable, "10.0.0.5:4000", true, false).unwrap_err();
        assert!(
            unreadable.to_string().contains("could not be read"),
            "{unreadable}"
        );
    }
}
