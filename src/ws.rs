//! RFC 6455 WebSocket frame codec.
//!
//! This is the wire layer used after a hyper HTTP/1.1 upgrade
//! (`101 Switching Protocols`). Hyper hands the relay a raw upgraded byte
//! stream; this module turns it into WebSocket frames.
//!
//! The reader is deliberately split into a *header* phase and a *payload*
//! phase. The server must reserve memory budget before a payload is allocated,
//! so [`FrameReader::read_header`] validates the frame and reports its payload
//! length without reading (or allocating) the payload. The caller can then
//! charge its budget and only afterwards call [`FrameReader::read_payload`].
//!
//! Only the server side of RFC 6455 is implemented:
//!
//! * inbound client frames must be masked; outbound server frames are not;
//! * no extensions: RSV1/RSV2/RSV3 must be zero and no compression is used;
//! * text/binary data frames plus close/ping/pong control frames.
//!
//! Upgrade-request validation and the `Sec-WebSocket-Accept` calculation live
//! here too because they belong to the same wire contract.

use std::io;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use http::header::{
    HeaderMap, HeaderName, CONNECTION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Magic GUID from RFC 6455 section 1.3.
pub const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Maximum payload of one masked client data frame.
///
/// This is the historical 32 MiB wire ceiling minus the 14-byte maximum masked
/// frame header (2-byte fixed header + 8-byte extended length + 4-byte mask).
pub const MAX_CLIENT_FRAME_PAYLOAD: usize = 32 * 1024 * 1024 - 14;

/// Per-connection frame cap for the v2 control socket.
///
/// This is a relay policy limit, not an RFC 6455 limit; RFC control frames are
/// still bounded to 125 payload bytes by [`FrameReader::read_header`].
pub const MAX_CONTROL_PAYLOAD: usize = 64 * 1024;

/// Errors produced by the frame codec and the upgrade validator.
#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("websocket I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("websocket protocol error: {0}")]
    Protocol(&'static str),

    #[error("websocket frame payload exceeds the configured maximum")]
    FrameTooLarge,

    #[error("websocket message payload exceeds the configured maximum")]
    MessageTooLarge,

    #[error("client frame is not masked")]
    InvalidMasking,

    #[error("invalid UTF-8 payload")]
    InvalidUtf8,

    #[error("invalid close payload")]
    InvalidClosePayload,

    #[error("websocket transport closed")]
    Closed,
}

impl WsError {
    /// Close code the server should send for this error.
    ///
    /// `1002` protocol error, `1007` invalid payload data, `1009` message too
    /// big, `1011` internal error, `1000` normal closure.
    pub fn close_code(&self) -> u16 {
        match self {
            WsError::Io(_) => 1011,
            WsError::Protocol(_) | WsError::InvalidMasking | WsError::InvalidClosePayload => 1002,
            WsError::FrameTooLarge | WsError::MessageTooLarge => 1009,
            WsError::InvalidUtf8 => 1007,
            WsError::Closed => 1000,
        }
    }
}

/// RFC 6455 frame opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum OpCode {
    Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    Close = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

impl OpCode {
    /// Map a raw opcode nibble, returning `None` for reserved opcodes.
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x0 => Some(OpCode::Continuation),
            0x1 => Some(OpCode::Text),
            0x2 => Some(OpCode::Binary),
            0x8 => Some(OpCode::Close),
            0x9 => Some(OpCode::Ping),
            0xA => Some(OpCode::Pong),
            _ => None,
        }
    }

    /// Raw opcode nibble.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A parsed frame header. The payload has not been read yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// FIN bit: last frame of a message.
    pub fin: bool,
    /// Frame opcode.
    pub opcode: OpCode,
    /// Payload length in bytes, already validated against the configured limits.
    pub payload_len: usize,
    /// Client masking key; the payload must be XORed with it.
    pub mask_key: [u8; 4],
}

/// Frame and message size limits applied while reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsLimits {
    /// Maximum payload of a single frame.
    pub max_frame_payload: usize,
    /// Maximum payload of one (possibly fragmented) message.
    pub max_message_payload: usize,
}

impl Default for WsLimits {
    fn default() -> Self {
        WsLimits {
            max_frame_payload: MAX_CLIENT_FRAME_PAYLOAD,
            max_message_payload: MAX_CLIENT_FRAME_PAYLOAD,
        }
    }
}

/// Streaming frame reader for frames sent by a client.
pub struct FrameReader<R> {
    reader: R,
    limits: WsLimits,
    /// Header whose payload has not been consumed yet.
    pending: Option<FrameHeader>,
    /// Assembled length of an in-progress fragmented message.
    fragmented_len: Option<usize>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Create a reader over `reader` with the given limits.
    pub fn new(reader: R, limits: WsLimits) -> Self {
        FrameReader {
            reader,
            limits,
            pending: None,
            fragmented_len: None,
        }
    }

    /// Read and validate the next frame header.
    ///
    /// This consumes the 2-byte fixed header, the extended length and the
    /// 4-byte client mask key, but never the payload. It rejects:
    ///
    /// * unmasked client frames ([`WsError::InvalidMasking`]);
    /// * non-zero RSV bits and reserved opcodes ([`WsError::Protocol`]);
    /// * frame payloads above `limits.max_frame_payload`
    ///   ([`WsError::FrameTooLarge`]) as soon as the length is known;
    /// * control frames with `fin = false` or payload > 125 bytes
    ///   ([`WsError::Protocol`]);
    /// * data messages above `limits.max_message_payload`
    ///   ([`WsError::MessageTooLarge`]) as soon as the length is known;
    /// * out-of-order fragmentation ([`WsError::Protocol`]).
    ///
    /// A clean EOF between frames yields [`WsError::Closed`].
    pub async fn read_header(&mut self) -> Result<FrameHeader, WsError> {
        if self.pending.is_some() {
            return Err(WsError::Protocol(
                "previous frame payload has not been read",
            ));
        }

        let mut head = [0u8; 2];
        self.read_fixed_header(&mut head).await?;

        let b0 = head[0];
        let b1 = head[1];

        let fin = b0 & 0x80 != 0;
        if b0 & 0x70 != 0 {
            return Err(WsError::Protocol("reserved bits must be zero"));
        }
        let opcode =
            OpCode::from_u8(b0 & 0x0F).ok_or(WsError::Protocol("reserved frame opcode"))?;

        if b1 & 0x80 == 0 {
            return Err(WsError::InvalidMasking);
        }
        let len7 = usize::from(b1 & 0x7F);

        let payload_len = if is_control(opcode) {
            if !fin {
                return Err(WsError::Protocol("control frames must not be fragmented"));
            }
            if len7 > 125 {
                return Err(WsError::Protocol("control frame payload exceeds 125 bytes"));
            }
            len7
        } else {
            self.read_extended_length(len7).await?
        };

        if payload_len > self.limits.max_frame_payload {
            return Err(WsError::FrameTooLarge);
        }

        // Track fragmented-message assembly limits without buffering the
        // message here; the caller still owns payload forwarding.
        match opcode {
            OpCode::Text | OpCode::Binary => {
                if self.fragmented_len.is_some() {
                    return Err(WsError::Protocol(
                        "new data frame while a fragmented message is in progress",
                    ));
                }
                if payload_len > self.limits.max_message_payload {
                    return Err(WsError::MessageTooLarge);
                }
                if !fin {
                    self.fragmented_len = Some(payload_len);
                }
            }
            OpCode::Continuation => {
                let previous = self
                    .fragmented_len
                    .ok_or(WsError::Protocol("continuation without an initial frame"))?;
                let total = previous
                    .checked_add(payload_len)
                    .ok_or(WsError::MessageTooLarge)?;
                if total > self.limits.max_message_payload {
                    return Err(WsError::MessageTooLarge);
                }
                self.fragmented_len = if fin { None } else { Some(total) };
            }
            OpCode::Close | OpCode::Ping | OpCode::Pong => {}
        }

        let mut mask_key = [0u8; 4];
        self.reader.read_exact(&mut mask_key).await?;

        let header = FrameHeader {
            fin,
            opcode,
            payload_len,
            mask_key,
        };
        self.pending = Some(header);
        Ok(header)
    }

    /// Read, unmask and return a frame payload.
    ///
    /// `header` must be the header returned by the most recent
    /// [`FrameReader::read_header`] call; it may be called only once per
    /// header. The allocation is exactly `header.payload_len` bytes.
    pub async fn read_payload(&mut self, header: &FrameHeader) -> Result<Bytes, WsError> {
        let pending = self
            .pending
            .take()
            .ok_or(WsError::Protocol("no frame header is awaiting a payload"))?;
        if pending != *header {
            return Err(WsError::Protocol(
                "frame header does not match the pending payload",
            ));
        }
        if header.payload_len == 0 {
            return Ok(Bytes::new());
        }

        let mut payload = vec![0u8; header.payload_len];
        self.reader.read_exact(&mut payload).await?;

        let mask = header.mask_key;
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index & 3];
        }
        Ok(Bytes::from(payload))
    }

    /// Read the 2-byte fixed header, mapping a clean EOF between frames to
    /// [`WsError::Closed`].
    async fn read_fixed_header(&mut self, head: &mut [u8; 2]) -> Result<(), WsError> {
        let mut filled = 0usize;
        while filled < head.len() {
            let read = self.reader.read(&mut head[filled..]).await?;
            if read == 0 {
                return if filled == 0 {
                    Err(WsError::Closed)
                } else {
                    Err(WsError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "unexpected end of stream in a WebSocket frame header",
                    )))
                };
            }
            filled += read;
        }
        Ok(())
    }

    /// Read and decode an extended payload length (RFC 6455 section 5.2).
    async fn read_extended_length(&mut self, len7: usize) -> Result<usize, WsError> {
        match len7 {
            126 => {
                let mut bytes = [0u8; 2];
                self.reader.read_exact(&mut bytes).await?;
                Ok(usize::from(u16::from_be_bytes(bytes)))
            }
            127 => {
                let mut bytes = [0u8; 8];
                self.reader.read_exact(&mut bytes).await?;
                let length = u64::from_be_bytes(bytes);
                if length & (1 << 63) != 0 {
                    return Err(WsError::Protocol(
                        "64-bit payload length has the high bit set",
                    ));
                }
                if length > self.limits.max_frame_payload as u64 {
                    return Err(WsError::FrameTooLarge);
                }
                usize::try_from(length).map_err(|_| WsError::FrameTooLarge)
            }
            length => Ok(length),
        }
    }
}

/// Streaming frame writer for server-to-client frames.
pub struct FrameWriter<W> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// Create a writer over `writer`.
    pub fn new(writer: W) -> Self {
        FrameWriter { writer }
    }

    /// Recover the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Write one unfragmented or fragmented server frame.
    ///
    /// Server-to-client frames are never masked. Control frames must be final
    /// and carry at most 125 payload bytes. Data frame payloads are capped at
    /// [`MAX_CLIENT_FRAME_PAYLOAD`] so the relay never emits a frame larger
    /// than the compatible wire ceiling. The frame is not flushed; call
    /// [`FrameWriter::flush`] when the caller is ready.
    pub async fn write_frame(
        &mut self,
        fin: bool,
        opcode: OpCode,
        payload: &[u8],
    ) -> Result<(), WsError> {
        if is_control(opcode) {
            if !fin {
                return Err(WsError::Protocol("control frames must not be fragmented"));
            }
            if payload.len() > 125 {
                return Err(WsError::Protocol("control frame payload exceeds 125 bytes"));
            }
        } else if payload.len() > MAX_CLIENT_FRAME_PAYLOAD {
            return Err(WsError::FrameTooLarge);
        }

        let mut header = [0u8; 10];
        header[0] = (if fin { 0x80 } else { 0x00 }) | opcode.as_u8();
        let length = payload.len();
        let header_len = if length <= 125 {
            header[1] = length as u8;
            2
        } else if length <= usize::from(u16::MAX) {
            header[1] = 126;
            header[2..4].copy_from_slice(&(length as u16).to_be_bytes());
            4
        } else {
            header[1] = 127;
            header[2..10].copy_from_slice(&(length as u64).to_be_bytes());
            10
        };

        self.writer.write_all(&header[..header_len]).await?;
        if !payload.is_empty() {
            self.writer.write_all(payload).await?;
        }
        Ok(())
    }

    /// Flush buffered bytes to the transport.
    pub async fn flush(&mut self) -> Result<(), WsError> {
        self.writer.flush().await?;
        Ok(())
    }

    /// Write a close frame carrying `code` and `reason`.
    ///
    /// The reason is truncated to at most 123 bytes on a UTF-8 boundary, so a
    /// long reason can never overflow the control-frame limit or panic.
    pub async fn write_close(&mut self, code: u16, reason: &str) -> Result<(), WsError> {
        let payload = close_payload(code, reason);
        self.write_frame(true, OpCode::Close, &payload).await
    }

    /// Write a ping frame.
    pub async fn write_ping(&mut self, payload: &[u8]) -> Result<(), WsError> {
        self.write_frame(true, OpCode::Ping, payload).await
    }

    /// Write a pong frame.
    pub async fn write_pong(&mut self, payload: &[u8]) -> Result<(), WsError> {
        self.write_frame(true, OpCode::Pong, payload).await
    }
}

/// Whether `op` is a WebSocket control opcode.
pub const fn is_control(op: OpCode) -> bool {
    matches!(op, OpCode::Close | OpCode::Ping | OpCode::Pong)
}

/// Build a close-frame payload: 2-byte status code plus a UTF-8 reason
/// truncated to at most 123 bytes (`125` total minus the code).
pub fn close_payload(code: u16, reason: &str) -> Bytes {
    let mut end = reason.len().min(123);
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut payload = BytesMut::with_capacity(2 + end);
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(&reason.as_bytes()[..end]);
    payload.freeze()
}

/// Parse a close-frame payload.
///
/// An empty payload means "no status code" and yields `(1005, "")`. A
/// one-byte payload is invalid. A reason that is not UTF-8 is rejected with
/// [`WsError::InvalidUtf8`].
pub fn parse_close_payload(payload: &[u8]) -> Result<(u16, String), WsError> {
    match payload.len() {
        0 => Ok((1005, String::new())),
        1 => Err(WsError::InvalidClosePayload),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            let reason = std::str::from_utf8(&payload[2..]).map_err(|_| WsError::InvalidUtf8)?;
            Ok((code, reason.to_owned()))
        }
    }
}

/// Build the `Sec-WebSocket-Accept` value: base64(sha1(key + GUID)).
pub fn accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(GUID.as_bytes());
    STANDARD.encode(hasher.finalize())
}

/// Errors from HTTP upgrade-request validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpgradeError {
    #[error("expected WebSocket upgrade")]
    NotUpgrade,
    #[error("invalid WebSocket key")]
    InvalidKey,
    #[error("unsupported WebSocket version")]
    InvalidVersion,
}

/// Validate the HTTP/1.1 headers of a WebSocket upgrade request.
///
/// Requires `Connection` to contain the `upgrade` token, `Upgrade` to contain
/// the `websocket` token, `Sec-WebSocket-Version: 13`, and a
/// `Sec-WebSocket-Key` that is standard padded Base64 for exactly 16 bytes.
/// Returns the raw key string for [`accept_key`].
pub fn validate_upgrade(headers: &HeaderMap) -> Result<String, UpgradeError> {
    if !header_has_token(headers, &CONNECTION, "upgrade")
        || !header_has_token(headers, &UPGRADE, "websocket")
    {
        return Err(UpgradeError::NotUpgrade);
    }

    let mut versions = headers.get_all(&SEC_WEBSOCKET_VERSION).iter();
    let version = versions.next();
    if versions.next().is_some() {
        return Err(UpgradeError::InvalidVersion);
    }
    match version.and_then(|value| value.to_str().ok()) {
        Some(value) if value.trim() == "13" => {}
        _ => return Err(UpgradeError::InvalidVersion),
    }

    let mut keys = headers.get_all(&SEC_WEBSOCKET_KEY).iter();
    let key = keys.next();
    if keys.next().is_some() {
        return Err(UpgradeError::InvalidKey);
    }
    let key = match key.and_then(|value| value.to_str().ok()) {
        Some(key) => key,
        None => return Err(UpgradeError::InvalidKey),
    };
    let decoded = STANDARD
        .decode(key.as_bytes())
        .map_err(|_| UpgradeError::InvalidKey)?;
    if decoded.len() != 16 {
        return Err(UpgradeError::InvalidKey);
    }
    Ok(key.to_owned())
}

/// Whether a header contains `token` in its comma-separated token list.
fn header_has_token(headers: &HeaderMap, name: &HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value
            .to_str()
            .map(|value| {
                value
                    .split(',')
                    .any(|candidate| candidate.trim().eq_ignore_ascii_case(token))
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;

    const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    fn encode_frame(fin: bool, opcode: OpCode, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 14);
        out.push((if fin { 0x80 } else { 0x00 }) | opcode.as_u8());
        let mask_bit = if mask.is_some() { 0x80 } else { 0x00 };
        let length = payload.len();
        if length <= 125 {
            out.push(mask_bit | length as u8);
        } else if length <= usize::from(u16::MAX) {
            out.push(mask_bit | 126);
            out.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            out.push(mask_bit | 127);
            out.extend_from_slice(&(length as u64).to_be_bytes());
        }
        if let Some(mask) = mask {
            out.extend_from_slice(&mask);
            out.extend(
                payload
                    .iter()
                    .enumerate()
                    .map(|(index, byte)| byte ^ mask[index & 3]),
            );
        } else {
            out.extend_from_slice(payload);
        }
        out
    }

    fn frame_reader(bytes: &[u8]) -> FrameReader<&[u8]> {
        FrameReader::new(bytes, WsLimits::default())
    }

    fn reader_with_limits(bytes: &[u8], limits: WsLimits) -> FrameReader<&[u8]> {
        FrameReader::new(bytes, limits)
    }

    fn decode_server_frame(bytes: &[u8]) -> (bool, OpCode, Vec<u8>) {
        assert!(!bytes.is_empty(), "empty server frame");
        let fin = bytes[0] & 0x80 != 0;
        let opcode = OpCode::from_u8(bytes[0] & 0x0F).expect("valid opcode");
        assert_eq!(bytes[1] & 0x80, 0, "server frames must not be masked");
        let len7 = usize::from(bytes[1] & 0x7F);
        let (length, offset) = match len7 {
            126 => (
                usize::from(u16::from_be_bytes([bytes[2], bytes[3]])),
                4usize,
            ),
            127 => {
                let value = u64::from_be_bytes(bytes[2..10].try_into().unwrap());
                (value as usize, 10usize)
            }
            value => (value, 2usize),
        };
        (fin, opcode, bytes[offset..offset + length].to_vec())
    }

    async fn write_one(fin: bool, opcode: OpCode, payload: &[u8]) -> Vec<u8> {
        let mut writer = FrameWriter::new(Vec::new());
        writer.write_frame(fin, opcode, payload).await.unwrap();
        writer.into_inner()
    }

    #[test]
    fn limits_constants_match_protocol_contract() {
        assert_eq!(MAX_CLIENT_FRAME_PAYLOAD, 33_554_418);
        assert_eq!(MAX_CONTROL_PAYLOAD, 65_536);
    }

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn opcode_mapping_roundtrips() {
        for (byte, opcode) in [
            (0x0, OpCode::Continuation),
            (0x1, OpCode::Text),
            (0x2, OpCode::Binary),
            (0x8, OpCode::Close),
            (0x9, OpCode::Ping),
            (0xA, OpCode::Pong),
        ] {
            assert_eq!(OpCode::from_u8(byte), Some(opcode));
            assert_eq!(opcode.as_u8(), byte);
        }
        for reserved in [0x3, 0x4, 0x5, 0x6, 0x7, 0xB, 0xC, 0xD, 0xE, 0xF] {
            assert_eq!(OpCode::from_u8(reserved), None);
        }
    }

    #[test]
    fn is_control_only_for_control_opcodes() {
        assert!(is_control(OpCode::Close));
        assert!(is_control(OpCode::Ping));
        assert!(is_control(OpCode::Pong));
        assert!(!is_control(OpCode::Continuation));
        assert!(!is_control(OpCode::Text));
        assert!(!is_control(OpCode::Binary));
    }

    #[tokio::test]
    async fn writer_encodes_small_text_frame() {
        let bytes = write_one(true, OpCode::Text, b"hello").await;
        assert_eq!(bytes, vec![0x81, 0x05, b'h', b'e', b'l', b'l', b'o']);
    }

    #[tokio::test]
    async fn writer_encodes_16bit_length() {
        let payload = vec![0xABu8; 200];
        let bytes = write_one(true, OpCode::Binary, &payload).await;
        assert_eq!(bytes.len(), 4 + payload.len());
        assert_eq!(&bytes[..4], &[0x82, 126, 0x00, 0xC8]);
        let (fin, opcode, decoded) = decode_server_frame(&bytes);
        assert!(fin);
        assert_eq!(opcode, OpCode::Binary);
        assert_eq!(decoded, payload);
    }

    #[tokio::test]
    async fn writer_encodes_64bit_length() {
        let payload = vec![0x5Au8; 70_000];
        let bytes = write_one(true, OpCode::Binary, &payload).await;
        assert_eq!(bytes.len(), 10 + payload.len());
        assert_eq!(bytes[1], 127);
        assert_eq!(&bytes[2..10], &70_000u64.to_be_bytes());
        let (_, opcode, decoded) = decode_server_frame(&bytes);
        assert_eq!(opcode, OpCode::Binary);
        assert_eq!(decoded, payload);
    }

    #[tokio::test]
    async fn writer_encodes_fragmented_frames() {
        let first = write_one(false, OpCode::Text, b"hel").await;
        assert_eq!(first, vec![0x01, 0x03, b'h', b'e', b'l']);
        let second = write_one(true, OpCode::Continuation, b"lo").await;
        assert_eq!(second, vec![0x80, 0x02, b'l', b'o']);
    }

    #[tokio::test]
    async fn reader_parses_masked_client_frame() {
        let frame = encode_frame(true, OpCode::Text, b"hello", Some(MASK));
        let mut reader = frame_reader(&frame);
        let header = reader.read_header().await.unwrap();
        assert!(header.fin);
        assert_eq!(header.opcode, OpCode::Text);
        assert_eq!(header.payload_len, 5);
        assert_eq!(header.mask_key, MASK);
        let payload = reader.read_payload(&header).await.unwrap();
        assert_eq!(payload, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn reader_parses_masked_binary_with_16bit_length() {
        let bytes: Vec<u8> = (0..=255u8).cycle().take(300).collect();
        let frame = encode_frame(true, OpCode::Binary, &bytes, Some(MASK));
        let mut reader = frame_reader(&frame);
        let header = reader.read_header().await.unwrap();
        assert_eq!(header.payload_len, bytes.len());
        let payload = reader.read_payload(&header).await.unwrap();
        assert_eq!(payload.as_ref(), bytes.as_slice());
    }

    #[tokio::test]
    async fn reader_parses_masked_64bit_length_without_reading_payload_early() {
        let bytes = vec![7u8; 70_000];
        let frame = encode_frame(true, OpCode::Binary, &bytes, Some(MASK));
        let mut reader = frame_reader(&frame);
        let header = reader.read_header().await.unwrap();
        assert_eq!(header.payload_len, bytes.len());
        let payload = reader.read_payload(&header).await.unwrap();
        assert_eq!(payload.len(), bytes.len());
        assert_eq!(payload.as_ref(), bytes.as_slice());
    }

    #[tokio::test]
    async fn reader_rejects_unmasked_client_frame() {
        let frame = encode_frame(true, OpCode::Text, b"hello", None);
        let mut reader = frame_reader(&frame);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::InvalidMasking));
        assert_eq!(error.close_code(), 1002);
    }

    #[tokio::test]
    async fn reader_rejects_rsv_bits() {
        let mut frame = encode_frame(true, OpCode::Text, b"x", Some(MASK));
        frame[0] |= 0x40; // RSV1
        let mut reader = frame_reader(&frame);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
        assert_eq!(error.close_code(), 1002);
    }

    #[tokio::test]
    async fn reader_rejects_reserved_opcode() {
        let frame = vec![0x83, 0x80, 0x00, 0x00, 0x00, 0x00];
        let mut reader = frame_reader(&frame);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
    }

    #[tokio::test]
    async fn reader_rejects_oversized_64bit_length_before_payload() {
        // Only the 10-byte masked header is present. If the codec tried to
        // read the payload it would hit EOF and return an I/O error instead.
        let mut frame = vec![0x81, 0xFF];
        frame.extend_from_slice(&((MAX_CLIENT_FRAME_PAYLOAD as u64) + 1).to_be_bytes());
        let mut reader = frame_reader(&frame);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::FrameTooLarge));
        assert_eq!(error.close_code(), 1009);
    }

    #[tokio::test]
    async fn reader_rejects_64bit_length_high_bit() {
        let mut frame = vec![0x81, 0xFF];
        frame.extend_from_slice(&(1u64 << 63).to_be_bytes());
        let mut reader = frame_reader(&frame);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
    }

    #[tokio::test]
    async fn reader_rejects_frame_above_configured_frame_limit() {
        let frame = encode_frame(true, OpCode::Binary, &[0u8; 200], Some(MASK));
        let limits = WsLimits {
            max_frame_payload: 100,
            max_message_payload: 100,
        };
        let mut reader = reader_with_limits(&frame, limits);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::FrameTooLarge));
    }

    #[tokio::test]
    async fn reader_rejects_message_above_configured_message_limit() {
        let frame = encode_frame(true, OpCode::Text, &[b'a'; 60], Some(MASK));
        let limits = WsLimits {
            max_frame_payload: 100,
            max_message_payload: 50,
        };
        let mut reader = reader_with_limits(&frame, limits);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::MessageTooLarge));
        assert_eq!(error.close_code(), 1009);
    }

    #[tokio::test]
    async fn reader_accumulates_fragmented_message_limit() {
        let mut bytes = encode_frame(false, OpCode::Text, &[b'a'; 3], Some(MASK));
        bytes.extend_from_slice(&encode_frame(
            true,
            OpCode::Continuation,
            &[b'b'; 3],
            Some(MASK),
        ));
        let limits = WsLimits {
            max_frame_payload: 100,
            max_message_payload: 5,
        };
        let mut reader = reader_with_limits(&bytes, limits);
        let first = reader.read_header().await.unwrap();
        assert!(!first.fin);
        reader.read_payload(&first).await.unwrap();
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::MessageTooLarge));
    }

    #[tokio::test]
    async fn reader_parses_fragmented_continuation() {
        let mut bytes = encode_frame(false, OpCode::Text, b"hel", Some(MASK));
        bytes.extend_from_slice(&encode_frame(true, OpCode::Continuation, b"lo", Some(MASK)));
        let mut reader = frame_reader(&bytes);

        let first = reader.read_header().await.unwrap();
        assert!(!first.fin);
        assert_eq!(first.opcode, OpCode::Text);
        assert_eq!(
            reader.read_payload(&first).await.unwrap(),
            Bytes::from_static(b"hel")
        );

        let second = reader.read_header().await.unwrap();
        assert!(second.fin);
        assert_eq!(second.opcode, OpCode::Continuation);
        assert_eq!(
            reader.read_payload(&second).await.unwrap(),
            Bytes::from_static(b"lo")
        );
    }

    #[tokio::test]
    async fn reader_allows_control_frames_between_fragments() {
        let mut bytes = encode_frame(false, OpCode::Text, b"ab", Some(MASK));
        bytes.extend_from_slice(&encode_frame(true, OpCode::Ping, b"p", Some(MASK)));
        bytes.extend_from_slice(&encode_frame(true, OpCode::Continuation, b"cd", Some(MASK)));
        let mut reader = frame_reader(&bytes);

        let first = reader.read_header().await.unwrap();
        reader.read_payload(&first).await.unwrap();

        let ping = reader.read_header().await.unwrap();
        assert_eq!(ping.opcode, OpCode::Ping);
        assert_eq!(
            reader.read_payload(&ping).await.unwrap(),
            Bytes::from_static(b"p")
        );

        let last = reader.read_header().await.unwrap();
        assert_eq!(last.opcode, OpCode::Continuation);
        reader.read_payload(&last).await.unwrap();
    }

    #[tokio::test]
    async fn reader_rejects_continuation_without_start() {
        let frame = encode_frame(true, OpCode::Continuation, b"x", Some(MASK));
        let mut reader = frame_reader(&frame);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
    }

    #[tokio::test]
    async fn reader_rejects_new_data_frame_during_fragmentation() {
        let mut bytes = encode_frame(false, OpCode::Text, b"a", Some(MASK));
        bytes.extend_from_slice(&encode_frame(true, OpCode::Text, b"b", Some(MASK)));
        let mut reader = frame_reader(&bytes);
        let first = reader.read_header().await.unwrap();
        reader.read_payload(&first).await.unwrap();
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
    }

    #[tokio::test]
    async fn reader_rejects_fragmented_control_frame() {
        let frame = encode_frame(false, OpCode::Ping, b"p", Some(MASK));
        let mut reader = frame_reader(&frame);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
    }

    #[tokio::test]
    async fn reader_rejects_control_frame_with_extended_length() {
        let mut frame = vec![0x89, 0x80 | 126, 0x00, 0x7E];
        frame.extend_from_slice(&MASK);
        frame.extend_from_slice(&[0u8; 126]);
        let mut reader = frame_reader(&frame);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
    }

    #[tokio::test]
    async fn reader_parses_control_frames() {
        let ping = encode_frame(true, OpCode::Ping, b"ping", Some(MASK));
        let pong = encode_frame(true, OpCode::Pong, b"pong", Some(MASK));
        let mut reader = frame_reader(&ping);
        let header = reader.read_header().await.unwrap();
        assert_eq!(header.opcode, OpCode::Ping);
        assert_eq!(
            reader.read_payload(&header).await.unwrap(),
            Bytes::from_static(b"ping")
        );

        let mut reader = frame_reader(&pong);
        let header = reader.read_header().await.unwrap();
        assert_eq!(header.opcode, OpCode::Pong);
        assert_eq!(
            reader.read_payload(&header).await.unwrap(),
            Bytes::from_static(b"pong")
        );
    }

    #[tokio::test]
    async fn reader_returns_closed_on_clean_eof_between_frames() {
        let mut reader = frame_reader(&[]);
        let error = reader.read_header().await.unwrap_err();
        assert!(matches!(error, WsError::Closed));
        assert_eq!(error.close_code(), 1000);
    }

    #[tokio::test]
    async fn reader_rejects_payload_double_read_and_mismatch() {
        let frame = encode_frame(true, OpCode::Text, b"hello", Some(MASK));
        let mut reader = frame_reader(&frame);
        let header = reader.read_header().await.unwrap();
        reader.read_payload(&header).await.unwrap();
        let error = reader.read_payload(&header).await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));

        let mut reader = frame_reader(&frame);
        let header = reader.read_header().await.unwrap();
        let wrong = FrameHeader {
            payload_len: header.payload_len + 1,
            ..header
        };
        let error = reader.read_payload(&wrong).await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
    }

    #[test]
    fn close_payload_encodes_code_and_reason() {
        assert_eq!(
            &close_payload(1000, "bye")[..],
            &[0x03, 0xE8, b'b', b'y', b'e']
        );
        assert_eq!(&close_payload(1000, "")[..], &[0x03, 0xE8]);
    }

    #[test]
    fn close_payload_truncates_ascii_reason() {
        let reason = "a".repeat(1_000);
        let payload = close_payload(1000, &reason);
        assert_eq!(payload.len(), 2 + 123);
        assert_eq!(&payload[2..], "a".repeat(123).as_bytes());
    }

    #[test]
    fn close_payload_truncates_on_utf8_boundary() {
        // 100 two-byte characters: 123 is not a char boundary for this string.
        let reason = "é".repeat(100);
        let payload = close_payload(1000, &reason);
        assert_eq!(payload.len(), 2 + 122);
        assert!(std::str::from_utf8(&payload[2..]).is_ok());
        let (code, decoded) = parse_close_payload(&payload).unwrap();
        assert_eq!(code, 1000);
        assert_eq!(decoded, "é".repeat(61));
    }

    #[test]
    fn parse_close_payload_handles_all_shapes() {
        assert_eq!(parse_close_payload(&[]).unwrap(), (1005, String::new()));
        assert_eq!(
            parse_close_payload(&[0x03, 0xE8, b'b', b'y', b'e']).unwrap(),
            (1000, "bye".to_owned())
        );
        assert!(matches!(
            parse_close_payload(&[0x03]).unwrap_err(),
            WsError::InvalidClosePayload
        ));
        assert!(matches!(
            parse_close_payload(&[0x03, 0xE8, 0xFF]).unwrap_err(),
            WsError::InvalidUtf8
        ));
        assert_eq!(
            parse_close_payload(&[0x03, 0xE8, 0xFF])
                .unwrap_err()
                .close_code(),
            1007
        );
    }

    #[tokio::test]
    async fn writer_roundtrips_close_frame() {
        let mut writer = FrameWriter::new(Vec::new());
        writer.write_close(1000, "done").await.unwrap();
        let bytes = writer.into_inner();
        let (fin, opcode, payload) = decode_server_frame(&bytes);
        assert!(fin);
        assert_eq!(opcode, OpCode::Close);
        assert_eq!(
            parse_close_payload(&payload).unwrap(),
            (1000, "done".to_owned())
        );
    }

    #[tokio::test]
    async fn writer_roundtrips_ping_and_pong() {
        let mut writer = FrameWriter::new(Vec::new());
        writer.write_ping(b"hi").await.unwrap();
        writer.write_pong(b"there").await.unwrap();
        writer.flush().await.unwrap();
        let bytes = writer.into_inner();

        let (_, first_opcode, first) = decode_server_frame(&bytes);
        assert_eq!(first_opcode, OpCode::Ping);
        assert_eq!(first, b"hi");
        let rest = &bytes[4..];
        let (_, second_opcode, second) = decode_server_frame(rest);
        assert_eq!(second_opcode, OpCode::Pong);
        assert_eq!(second, b"there");
    }

    #[tokio::test]
    async fn writer_rejects_oversized_control_payloads() {
        let mut writer = FrameWriter::new(Vec::new());
        let error = writer
            .write_frame(true, OpCode::Ping, &[0u8; 126])
            .await
            .unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
        let error = writer.write_ping(&[0u8; 126]).await.unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
        let error = writer
            .write_frame(false, OpCode::Close, b"x")
            .await
            .unwrap_err();
        assert!(matches!(error, WsError::Protocol(_)));
    }

    #[tokio::test]
    async fn writer_rejects_payload_above_wire_ceiling() {
        // Construct the over-limit slice without allocating the full payload.
        let too_large = vec![0u8; MAX_CLIENT_FRAME_PAYLOAD + 1];
        let mut writer = FrameWriter::new(Vec::new());
        let error = writer
            .write_frame(true, OpCode::Binary, &too_large)
            .await
            .unwrap_err();
        assert!(matches!(error, WsError::FrameTooLarge));
    }

    #[test]
    fn validate_upgrade_accepts_rfc_example() {
        let headers = upgrade_headers();
        assert_eq!(
            validate_upgrade(&headers).unwrap(),
            "dGhlIHNhbXBsZSBub25jZQ=="
        );
    }

    #[test]
    fn validate_upgrade_is_case_insensitive_for_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("Keep-Alive, UPGRADE"));
        headers.insert(UPGRADE, HeaderValue::from_static("WebSocket"));
        headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
        headers.insert(
            SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        assert!(validate_upgrade(&headers).is_ok());
    }

    #[test]
    fn validate_upgrade_rejects_missing_connection_token() {
        let mut headers = upgrade_headers();
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        assert_eq!(validate_upgrade(&headers), Err(UpgradeError::NotUpgrade));
    }

    #[test]
    fn validate_upgrade_rejects_missing_upgrade_header() {
        let mut headers = upgrade_headers();
        headers.remove(UPGRADE);
        assert_eq!(validate_upgrade(&headers), Err(UpgradeError::NotUpgrade));

        headers.insert(UPGRADE, HeaderValue::from_static("h2c"));
        assert_eq!(validate_upgrade(&headers), Err(UpgradeError::NotUpgrade));
    }

    #[test]
    fn validate_upgrade_rejects_bad_version() {
        let mut headers = upgrade_headers();
        headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("8"));
        assert_eq!(
            validate_upgrade(&headers),
            Err(UpgradeError::InvalidVersion)
        );

        headers.remove(SEC_WEBSOCKET_VERSION);
        assert_eq!(
            validate_upgrade(&headers),
            Err(UpgradeError::InvalidVersion)
        );
    }

    #[test]
    fn validate_upgrade_rejects_bad_key() {
        let mut headers = upgrade_headers();
        headers.insert(SEC_WEBSOCKET_KEY, HeaderValue::from_static("not base64"));
        assert_eq!(validate_upgrade(&headers), Err(UpgradeError::InvalidKey));

        // Valid base64 but only 15 decoded bytes.
        headers.insert(
            SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAA"),
        );
        assert_eq!(validate_upgrade(&headers), Err(UpgradeError::InvalidKey));

        // Unpadded base64 is rejected.
        headers.insert(
            SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ"),
        );
        assert_eq!(validate_upgrade(&headers), Err(UpgradeError::InvalidKey));

        headers.remove(SEC_WEBSOCKET_KEY);
        assert_eq!(validate_upgrade(&headers), Err(UpgradeError::InvalidKey));
    }

    fn upgrade_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
        headers.insert(
            SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers
    }
}
