//! Paseo relay connection parameters and validation.
//!
//! `serverId`, `role`, `v`, `connectionId` come from the WebSocket URL query.
//! They are treated as routing metadata only. `serverId` is a bearer-style
//! routing key (Paseo generates 72 random bits), not an authentication token.

use std::fmt;

use thiserror::Error;

/// Maximum accepted byte length for `serverId` and `connectionId`.
pub const MAX_ROUTE_ID_BYTES: usize = 256;

/// Relay routing version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
    V2,
}

impl Version {
    pub fn as_u8(self) -> u8 {
        match self {
            Version::V1 => 1,
            Version::V2 => 2,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_u8())
    }
}

/// Which side of a Paseo session opened this WebSocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Client => "client",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated `/ws` route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub server_id: String,
    pub role: Role,
    pub version: Version,
    /// `None` for v1. For v2 this is always `Some`.
    ///
    /// An empty string identifies the daemon's control socket and is only
    /// valid for `role=server`; omitting `connectionId` (or sending it empty)
    /// is the documented way to open the control socket. A v2 `role=client`
    /// that omits `connectionId` (or sends it empty) is assigned a
    /// relay-generated id. Query values are used verbatim: they are never
    /// trimmed, and a value that is only whitespace is rejected rather than
    /// treated as empty.
    pub connection_id: Option<String>,
}

impl Connection {
    /// Parse and validate the query string of a `/ws` upgrade.
    pub fn from_query(query: &str) -> Result<Self, ConnectionError> {
        let mut role = None;
        let mut server_id = None;
        let mut version = None;
        let mut connection_id = None;

        // Last value wins, matching `Map.new/1` in the Elixir implementation.
        // Decode manually so invalid UTF-8 in a percent-encoded value is a hard
        // 400 instead of being silently replaced by U+FFFD.
        for part in query.split('&') {
            if part.is_empty() {
                continue;
            }
            let (raw_key, raw_value) = part.split_once('=').unwrap_or((part, ""));
            let key = decode_component(raw_key).ok_or(ConnectionError::Encoding)?;
            let value = decode_component(raw_value).ok_or(ConnectionError::Encoding)?;
            match key.as_str() {
                "role" => role = Some(value),
                "serverId" => server_id = Some(value),
                "v" => version = Some(value),
                "connectionId" => connection_id = Some(value),
                _ => {}
            }
        }

        let role = match role.as_deref() {
            Some("server") => Role::Server,
            Some("client") => Role::Client,
            _ => return Err(ConnectionError::Role),
        };

        let server_id = server_id.ok_or(ConnectionError::MissingServerId)?;
        validate_route_id(&server_id, IdKind::ServerId)?;

        // `v` is matched verbatim: RFC 3986 query values arrive already
        // decoded, and a value such as `" 2"`/`"2 "` is a client error rather
        // than a second spelling of v2. Only absent/empty keep the v1 default.
        let version = match version.as_deref() {
            None | Some("") | Some("1") => Version::V1,
            Some("2") => Version::V2,
            Some(_) => return Err(ConnectionError::Version),
        };

        let connection_id = match version {
            Version::V1 => None,
            Version::V2 => {
                // Used verbatim as well: pads are not stripped, so `%20`
                // fails route-id validation below instead of silently turning
                // the control socket's empty id into a data route.
                let value = connection_id.as_deref().unwrap_or("").to_owned();
                if value.len() > MAX_ROUTE_ID_BYTES {
                    return Err(ConnectionError::ConnectionIdTooLong);
                }
                if !value.is_empty() {
                    validate_route_id(&value, IdKind::ConnectionId)?;
                }
                if value.is_empty() && role == Role::Client {
                    Some(generated_connection_id())
                } else {
                    Some(value)
                }
            }
        };

        Ok(Connection {
            server_id,
            role,
            version,
            connection_id,
        })
    }

    pub fn is_v2_control(&self) -> bool {
        self.version == Version::V2
            && self.role == Role::Server
            && self.connection_id.as_deref() == Some("")
    }

    pub fn is_v2_data(&self) -> bool {
        self.version == Version::V2
            && self.role == Role::Server
            && self
                .connection_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
    }

    pub fn is_v2_client(&self) -> bool {
        self.version == Version::V2 && self.role == Role::Client
    }

    /// Connection id for v2 sockets. Always present for v2.
    pub fn connection_id(&self) -> Option<&str> {
        self.connection_id.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdKind {
    ServerId,
    ConnectionId,
}

fn validate_route_id(value: &str, kind: IdKind) -> Result<(), ConnectionError> {
    if value.is_empty() {
        return Err(match kind {
            IdKind::ServerId => ConnectionError::MissingServerId,
            IdKind::ConnectionId => ConnectionError::ConnectionId,
        });
    }
    if value.len() > MAX_ROUTE_ID_BYTES {
        return Err(match kind {
            IdKind::ServerId => ConnectionError::ServerIdTooLong,
            IdKind::ConnectionId => ConnectionError::ConnectionIdTooLong,
        });
    }
    // Reject control characters and whitespace (Rust `str` already guarantees
    // valid UTF-8). The query decoder never lossily replaces bytes: invalid
    // percent-encoding or invalid UTF-8 makes the whole parse fail with
    // `ConnectionError::Encoding`. Route ids still get this check because they
    // appear in logs at edges, are used as session keys, and are embedded in
    // JSON control frames.
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(match kind {
            IdKind::ServerId => ConnectionError::ServerId,
            IdKind::ConnectionId => ConnectionError::ConnectionId,
        });
    }
    Ok(())
}

fn decode_component(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hi = *bytes.get(index + 1)?;
                let lo = *bytes.get(index + 2)?;
                out.push((hex_value(hi)? << 4) | hex_value(lo)?);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn generated_connection_id() -> String {
    // 8 random bytes, lower-case hex. The relay assigns this when a v2 client
    // omits connectionId, matching the Paseo relay contract.
    format!("conn_{:016x}", rand::random::<u64>())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConnectionError {
    #[error("Missing or invalid role parameter")]
    Role,
    #[error("Missing serverId parameter")]
    MissingServerId,
    #[error("serverId is too long")]
    ServerIdTooLong,
    #[error("Invalid serverId parameter")]
    ServerId,
    #[error("Invalid v parameter (expected 1 or 2)")]
    Version,
    #[error("connectionId is too long")]
    ConnectionIdTooLong,
    #[error("Invalid connectionId parameter")]
    ConnectionId,
    #[error("Invalid UTF-8 or percent-encoding in query parameters")]
    Encoding,
}

impl ConnectionError {
    /// HTTP status used for a rejected query string.
    pub fn status(&self) -> u16 {
        400
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `application/x-www-form-urlencoded` query from key/value pairs.
    fn q(pairs: &[(&str, &str)]) -> String {
        fn encode(value: &str) -> String {
            value
                .bytes()
                .map(|byte| match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                        char::from(byte).to_string()
                    }
                    b' ' => "+".to_owned(),
                    other => format!("%{other:02X}"),
                })
                .collect()
        }
        pairs
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&")
    }

    #[test]
    fn parses_v1_defaults() {
        let c = Connection::from_query(&q(&[("serverId", "srv_1"), ("role", "client")])).unwrap();
        assert_eq!(c.version, Version::V1);
        assert_eq!(c.role, Role::Client);
        assert_eq!(c.connection_id, None);
    }

    #[test]
    fn parses_v2_control_and_data() {
        let control =
            Connection::from_query(&q(&[("serverId", "srv_1"), ("role", "server"), ("v", "2")]))
                .unwrap();
        assert!(control.is_v2_control());
        assert_eq!(control.connection_id.as_deref(), Some(""));

        let data = Connection::from_query(&q(&[
            ("serverId", "srv_1"),
            ("role", "server"),
            ("v", "2"),
            ("connectionId", "abc"),
        ]))
        .unwrap();
        assert!(data.is_v2_data());
        assert_eq!(data.connection_id.as_deref(), Some("abc"));
    }

    #[test]
    fn generates_client_connection_id() {
        let c =
            Connection::from_query(&q(&[("serverId", "srv_1"), ("role", "client"), ("v", "2")]))
                .unwrap();
        let id = c.connection_id().unwrap();
        assert!(id.starts_with("conn_"));
        assert_eq!(id.len(), 5 + 16);
    }

    #[test]
    fn rejects_bad_inputs() {
        assert_eq!(
            Connection::from_query("").unwrap_err(),
            ConnectionError::Role
        );
        assert!(matches!(
            Connection::from_query(&q(&[("serverId", "x"), ("role", "client"), ("v", "3")])),
            Err(ConnectionError::Version)
        ));
        assert!(matches!(
            Connection::from_query(&q(&[("role", "client")])),
            Err(ConnectionError::MissingServerId)
        ));
        let long = "x".repeat(MAX_ROUTE_ID_BYTES + 1);
        assert!(matches!(
            Connection::from_query(&q(&[("serverId", &long), ("role", "client")])),
            Err(ConnectionError::ServerIdTooLong)
        ));
        assert!(matches!(
            Connection::from_query(&q(&[
                ("serverId", "x"),
                ("role", "client"),
                ("v", "2"),
                ("connectionId", "bad\nid")
            ])),
            Err(ConnectionError::ConnectionId)
        ));
    }

    #[test]
    fn version_and_connection_id_are_not_trimmed() {
        // A padded version is not v2; the query decoder maps `+` to a space,
        // so `v=+2` used to be accepted as v2 by the old `.trim()`.
        assert_eq!(
            Connection::from_query("serverId=srv_1&role=server&v=+2").unwrap_err(),
            ConnectionError::Version
        );
        assert_eq!(
            Connection::from_query(&q(&[
                ("serverId", "srv_1"),
                ("role", "server"),
                ("v", "2 ")
            ]))
            .unwrap_err(),
            ConnectionError::Version
        );
        // A whitespace-only connectionId is rejected instead of being
        // trimmed into the control socket's empty id.
        assert!(matches!(
            Connection::from_query("serverId=srv_1&role=server&v=2&connectionId=%20"),
            Err(ConnectionError::ConnectionId)
        ));
        assert!(matches!(
            Connection::from_query(&q(&[
                ("serverId", "srv_1"),
                ("role", "client"),
                ("v", "2"),
                ("connectionId", " "),
            ])),
            Err(ConnectionError::ConnectionId)
        ));
    }

    #[test]
    fn empty_connection_id_is_control_for_servers_and_generated_for_clients() {
        // Documented empty-value semantics: for `role=server`, an omitted or
        // empty connectionId is the control socket.
        let omitted = Connection::from_query("serverId=srv&role=server&v=2").unwrap();
        assert!(omitted.is_v2_control());
        let empty = Connection::from_query("serverId=srv&role=server&v=2&connectionId=").unwrap();
        assert!(empty.is_v2_control());
        // For `role=client`, the relay assigns an id instead.
        let omitted = Connection::from_query("serverId=srv&role=client&v=2").unwrap();
        assert!(omitted.connection_id().is_some_and(|id| !id.is_empty()));
        let empty = Connection::from_query("serverId=srv&role=client&v=2&connectionId=").unwrap();
        assert!(empty.connection_id().is_some_and(|id| !id.is_empty()));
    }

    #[test]
    fn duplicate_keys_use_last_value() {
        let c = Connection::from_query(&q(&[
            ("serverId", "first"),
            ("serverId", "second"),
            ("role", "client"),
        ]))
        .unwrap();
        assert_eq!(c.server_id, "second");
    }

    #[test]
    fn rejects_invalid_utf8_percent_encoding() {
        assert!(matches!(
            Connection::from_query("serverId=%FF&role=client&v=2&connectionId=x"),
            Err(ConnectionError::Encoding)
        ));
    }

    #[test]
    fn decodes_plus_and_percent() {
        let c = Connection::from_query("serverId=a%2Fb&role=client").unwrap();
        assert_eq!(c.server_id, "a/b");
    }

    #[test]
    fn v1_ignores_connection_id() {
        let c = Connection::from_query(&q(&[
            ("serverId", "x"),
            ("role", "client"),
            ("connectionId", "ignored"),
        ]))
        .unwrap();
        assert_eq!(c.connection_id, None);
    }
}
