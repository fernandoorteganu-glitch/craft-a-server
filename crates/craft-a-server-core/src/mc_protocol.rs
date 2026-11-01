//! Minecraft Java "Server List Ping" (status) protocol, pure and I/O free.
//!
//! The daemon uses it as the health check ("did the server come up?"). Replies
//! come from a process that may be a hostile mod, so every length is bounded
//! and nothing here panics on malformed input (see the fuzz target).

use serde::Deserialize;

/// Upper bound on a whole status packet (favicons make replies large).
pub const MAX_PACKET_LEN: usize = 1 << 20;
/// Protocol number sent in the handshake. Servers answer status requests
/// regardless of it; `-1` is what the vanilla client sends when probing.
const PROBE_PROTOCOL: i32 = -1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("incomplete data")]
    Incomplete,
    #[error("malformed VarInt")]
    BadVarInt,
    #[error("packet length {0} out of range")]
    BadLength(i32),
    #[error("unexpected packet id {0}")]
    UnexpectedPacket(i32),
    #[error("status JSON invalid: {0}")]
    BadJson(String),
}

/// Appends `value` as a Minecraft VarInt.
pub fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut v = value as u32;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Reads a VarInt from the start of `buf`, returning `(value, bytes_used)`.
pub fn read_varint(buf: &[u8]) -> Result<(i32, usize), ProtocolError> {
    let mut result: u32 = 0;
    for (i, byte) in buf.iter().take(5).enumerate() {
        result |= u32::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((result as i32, i + 1));
        }
    }
    if buf.len() >= 5 {
        Err(ProtocolError::BadVarInt)
    } else {
        Err(ProtocolError::Incomplete)
    }
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    // Payloads built here are tiny; the cast cannot truncate.
    write_varint(&mut out, payload.len() as i32);
    out.extend_from_slice(payload);
    out
}

/// Bytes to send to request the status: handshake (next state 1) followed by
/// the empty status request.
pub fn status_request(host: &str, port: u16) -> Vec<u8> {
    let mut hs = Vec::new();
    write_varint(&mut hs, 0x00);
    write_varint(&mut hs, PROBE_PROTOCOL);
    write_varint(&mut hs, host.len() as i32);
    hs.extend_from_slice(host.as_bytes());
    hs.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut hs, 1);

    let mut out = frame(&hs);
    out.extend_from_slice(&frame(&[0x00]));
    out
}

/// What the panel shows about a running server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub version_name: String,
    pub protocol: i64,
    pub players_online: u32,
    pub players_max: u32,
}

#[derive(Deserialize)]
struct RawStatus {
    version: RawVersion,
    players: RawPlayers,
}

#[derive(Deserialize)]
struct RawVersion {
    name: String,
    protocol: i64,
}

#[derive(Deserialize)]
struct RawPlayers {
    max: u32,
    online: u32,
}

/// How many bytes of `buf` the first complete packet needs, if known yet.
/// Returns `Ok(None)` when more bytes are required to decide.
pub fn packet_len(buf: &[u8]) -> Result<Option<usize>, ProtocolError> {
    match read_varint(buf) {
        Ok((len, used)) => {
            let len = usize::try_from(len).map_err(|_| ProtocolError::BadLength(len))?;
            if len == 0 || len > MAX_PACKET_LEN {
                return Err(ProtocolError::BadLength(len as i32));
            }
            Ok(Some(used + len))
        }
        Err(ProtocolError::Incomplete) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Parses a complete status response packet (length prefix included).
pub fn parse_status_response(buf: &[u8]) -> Result<ServerStatus, ProtocolError> {
    let total = packet_len(buf)?.ok_or(ProtocolError::Incomplete)?;
    let packet = buf.get(..total).ok_or(ProtocolError::Incomplete)?;
    let (_, len_used) = read_varint(packet)?;
    let body = packet.get(len_used..).ok_or(ProtocolError::Incomplete)?;

    let (id, id_used) = read_varint(body)?;
    if id != 0x00 {
        return Err(ProtocolError::UnexpectedPacket(id));
    }
    let rest = body.get(id_used..).ok_or(ProtocolError::Incomplete)?;
    let (str_len, str_used) = read_varint(rest)?;
    let str_len = usize::try_from(str_len).map_err(|_| ProtocolError::BadLength(str_len))?;
    let json = rest
        .get(str_used..str_used.saturating_add(str_len))
        .ok_or(ProtocolError::Incomplete)?;

    let raw: RawStatus =
        serde_json::from_slice(json).map_err(|e| ProtocolError::BadJson(e.to_string()))?;
    Ok(ServerStatus {
        version_name: raw.version.name,
        protocol: raw.version.protocol,
        players_online: raw.players.online,
        players_max: raw.players.max,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(json: &str) -> Vec<u8> {
        let mut body = Vec::new();
        write_varint(&mut body, 0x00);
        write_varint(&mut body, json.len() as i32);
        body.extend_from_slice(json.as_bytes());
        frame(&body)
    }

    #[test]
    fn varint_known_values() {
        for (value, bytes) in [
            (0, vec![0x00]),
            (1, vec![0x01]),
            (127, vec![0x7f]),
            (128, vec![0x80, 0x01]),
            (25565, vec![0xdd, 0xc7, 0x01]),
            (2_147_483_647, vec![0xff, 0xff, 0xff, 0xff, 0x07]),
            (-1, vec![0xff, 0xff, 0xff, 0xff, 0x0f]),
        ] {
            let mut out = Vec::new();
            write_varint(&mut out, value);
            assert_eq!(out, bytes, "encode {value}");
            assert_eq!(
                read_varint(&bytes),
                Ok((value, bytes.len())),
                "decode {value}"
            );
        }
    }

    #[test]
    fn varint_errors() {
        assert_eq!(read_varint(&[]), Err(ProtocolError::Incomplete));
        assert_eq!(read_varint(&[0x80]), Err(ProtocolError::Incomplete));
        assert_eq!(
            read_varint(&[0x80, 0x80, 0x80, 0x80, 0x80]),
            Err(ProtocolError::BadVarInt)
        );
    }

    #[test]
    fn request_layout() -> Result<(), ProtocolError> {
        let req = status_request("127.0.0.1", 25565);
        // handshake frame, then the 2-byte status request frame.
        assert!(req.ends_with(&[0x01, 0x00]));
        let (len, used) = read_varint(&req)?;
        assert_eq!(len as usize + used + 2, req.len());
        Ok(())
    }

    #[test]
    fn parses_real_fabric_reply() {
        let json = r#"{"version":{"name":"26.3","protocol":777},"players":{"max":20,"online":0},"description":"A Minecraft Server"}"#;
        let status = parse_status_response(&response(json));
        assert_eq!(
            status,
            Ok(ServerStatus {
                version_name: "26.3".into(),
                protocol: 777,
                players_online: 0,
                players_max: 20,
            })
        );
    }

    #[test]
    fn tolerates_sample_and_extra_fields() {
        let json = r#"{"version":{"name":"x","protocol":1},"players":{"max":5,"online":1,"sample":[{"name":"a","id":"b"}]},"favicon":"data:image/png;base64,AAAA","enforcesSecureChat":true}"#;
        assert!(parse_status_response(&response(json)).is_ok());
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_status_response(&[]), Err(ProtocolError::Incomplete));
        assert!(matches!(
            parse_status_response(&[0x00]),
            Err(ProtocolError::BadLength(0))
        ));
        // legacy ping reply starts with 0xFF
        assert!(parse_status_response(&[0xff, 0x00, 0x10]).is_err());
        // wrong packet id
        let mut body = Vec::new();
        write_varint(&mut body, 0x05);
        assert_eq!(
            parse_status_response(&frame(&body)),
            Err(ProtocolError::UnexpectedPacket(5))
        );
        assert!(matches!(
            parse_status_response(&response("not json")),
            Err(ProtocolError::BadJson(_))
        ));
    }

    #[test]
    fn rejects_oversized_length() {
        let mut buf = Vec::new();
        write_varint(&mut buf, (MAX_PACKET_LEN + 1) as i32);
        assert!(matches!(packet_len(&buf), Err(ProtocolError::BadLength(_))));
    }

    #[test]
    fn truncated_packet_is_incomplete() {
        let full =
            response(r#"{"version":{"name":"1","protocol":1},"players":{"max":1,"online":0}}"#);
        let cut = full.get(..full.len() - 3).unwrap_or(&[]);
        assert_eq!(parse_status_response(cut), Err(ProtocolError::Incomplete));
    }

    proptest::proptest! {
        #[test]
        fn never_panics(data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..2048)) {
            let _ = parse_status_response(&data);
            let _ = packet_len(&data);
            let _ = read_varint(&data);
        }

        #[test]
        fn varint_roundtrip(v in proptest::prelude::any::<i32>()) {
            let mut out = Vec::new();
            write_varint(&mut out, v);
            proptest::prop_assert_eq!(read_varint(&out), Ok((v, out.len())));
        }
    }
}
