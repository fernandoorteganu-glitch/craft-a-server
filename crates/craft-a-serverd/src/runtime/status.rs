//! Real Server List Ping client built on the pure helpers in
//! `craft_a_server_core::mc_protocol`.

use std::time::Duration;

use craft_a_server_core::mc_protocol::{
    MAX_PACKET_LEN, ProtocolError, ServerStatus, packet_len, parse_status_response, status_request,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("status ping timed out")]
    Timeout,
    #[error("connection failed: {0}")]
    Io(String),
    #[error("connection closed before a full reply")]
    Closed,
    #[error("invalid reply: {0}")]
    Protocol(#[from] ProtocolError),
}

/// Pings `host:port` and returns the parsed status. `timeout` bounds the whole
/// exchange (connect, send, read). The reply is read incrementally and never
/// buffered beyond the protocol's maximum packet size.
pub async fn mc_status(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<ServerStatus, StatusError> {
    tokio::time::timeout(timeout, ping(host, port))
        .await
        .map_err(|_| StatusError::Timeout)?
}

async fn ping(host: &str, port: u16) -> Result<ServerStatus, StatusError> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .map_err(|e| StatusError::Io(e.to_string()))?;
    stream
        .write_all(&status_request(host, port))
        .await
        .map_err(|e| StatusError::Io(e.to_string()))?;

    // Largest acceptable reply: length prefix (<= 5 bytes) + MAX_PACKET_LEN.
    let hard_cap = MAX_PACKET_LEN + 5;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(total) = packet_len(&buf)?
            && buf.len() >= total
        {
            return Ok(parse_status_response(&buf)?);
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| StatusError::Io(e.to_string()))?;
        if n == 0 {
            return Err(StatusError::Closed);
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        if buf.len() > hard_cap {
            return Err(StatusError::Protocol(ProtocolError::BadLength(-1)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_a_server_core::mc_protocol::write_varint;
    use tokio::net::TcpListener;

    fn reply(json: &str) -> Vec<u8> {
        let mut body = Vec::new();
        write_varint(&mut body, 0x00);
        write_varint(&mut body, json.len() as i32);
        body.extend_from_slice(json.as_bytes());
        let mut out = Vec::new();
        write_varint(&mut out, body.len() as i32);
        out.extend_from_slice(&body);
        out
    }

    /// Serves one connection: reads the request, then runs `script`.
    async fn fake_server<F, Fut>(script: F) -> Result<u16, std::io::Error>
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut req = [0u8; 256];
                let _ = s.read(&mut req).await;
                script(s).await;
            }
        });
        Ok(port)
    }

    const JSON: &str = r#"{"version":{"name":"1.21.4","protocol":769},"players":{"max":20,"online":3},"description":"hi"}"#;

    #[tokio::test]
    async fn valid_reply() -> Result<(), Box<dyn std::error::Error>> {
        let port = fake_server(|mut s| async move {
            let _ = s.write_all(&reply(JSON)).await;
        })
        .await?;
        let st = mc_status("127.0.0.1", port, Duration::from_secs(5)).await?;
        assert_eq!(st.version_name, "1.21.4");
        assert_eq!(st.players_online, 3);
        assert_eq!(st.players_max, 20);
        Ok(())
    }

    #[tokio::test]
    async fn reply_in_small_pieces() -> Result<(), Box<dyn std::error::Error>> {
        let port = fake_server(|mut s| async move {
            for piece in reply(JSON).chunks(7) {
                let _ = s.write_all(piece).await;
                let _ = s.flush().await;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await?;
        let st = mc_status("127.0.0.1", port, Duration::from_secs(5)).await?;
        assert_eq!(st.players_max, 20);
        Ok(())
    }

    #[tokio::test]
    async fn garbage_reply() -> Result<(), Box<dyn std::error::Error>> {
        let port = fake_server(|mut s| async move {
            let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        })
        .await?;
        let r = mc_status("127.0.0.1", port, Duration::from_secs(5)).await;
        // 'H' reads as a 72-byte packet, so the short reply ends in a close.
        assert!(
            matches!(r, Err(StatusError::Closed | StatusError::Protocol(_))),
            "{r:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn zero_length_packet_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let port = fake_server(|mut s| async move {
            let _ = s.write_all(&[0x00, 0x00, 0x00]).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        })
        .await?;
        let r = mc_status("127.0.0.1", port, Duration::from_secs(5)).await;
        assert!(matches!(r, Err(StatusError::Protocol(_))), "{r:?}");
        Ok(())
    }

    #[tokio::test]
    async fn bad_json_reply() -> Result<(), Box<dyn std::error::Error>> {
        let port = fake_server(|mut s| async move {
            let _ = s.write_all(&reply("{not json")).await;
        })
        .await?;
        let r = mc_status("127.0.0.1", port, Duration::from_secs(5)).await;
        assert!(matches!(r, Err(StatusError::Protocol(_))), "{r:?}");
        Ok(())
    }

    #[tokio::test]
    async fn slow_reply_times_out() -> Result<(), Box<dyn std::error::Error>> {
        let port = fake_server(|s| async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(s);
        })
        .await?;
        let started = std::time::Instant::now();
        let r = mc_status("127.0.0.1", port, Duration::from_millis(300)).await;
        assert!(matches!(r, Err(StatusError::Timeout)), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
        Ok(())
    }

    #[tokio::test]
    async fn huge_length_is_rejected_without_buffering() -> Result<(), Box<dyn std::error::Error>> {
        let port = fake_server(|mut s| async move {
            let mut out = Vec::new();
            write_varint(&mut out, i32::MAX);
            let _ = s.write_all(&out).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        })
        .await?;
        let r = mc_status("127.0.0.1", port, Duration::from_secs(5)).await;
        assert!(matches!(r, Err(StatusError::Protocol(_))), "{r:?}");
        Ok(())
    }

    #[tokio::test]
    async fn connection_closed_early() -> Result<(), Box<dyn std::error::Error>> {
        let port = fake_server(|mut s| async move {
            // Half a packet, then hang up.
            let full = reply(JSON);
            let _ = s.write_all(full.get(..10).unwrap_or_default()).await;
        })
        .await?;
        let r = mc_status("127.0.0.1", port, Duration::from_secs(5)).await;
        assert!(matches!(r, Err(StatusError::Closed)), "{r:?}");
        Ok(())
    }

    #[tokio::test]
    async fn connection_refused() -> Result<(), Box<dyn std::error::Error>> {
        let port = {
            let l = TcpListener::bind(("127.0.0.1", 0)).await?;
            l.local_addr()?.port()
        };
        let r = mc_status("127.0.0.1", port, Duration::from_secs(5)).await;
        assert!(matches!(r, Err(StatusError::Io(_))), "{r:?}");
        Ok(())
    }
}
