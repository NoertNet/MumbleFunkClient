//! Mumble control-channel framing: `u16 type | u32 length | payload`, big endian.

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest accepted frame. Mumble's own limit is 8 MiB.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageType {
    Version = 0,
    UdpTunnel = 1,
    Authenticate = 2,
    Ping = 3,
    Reject = 4,
    ServerSync = 5,
    ChannelRemove = 6,
    ChannelState = 7,
    UserRemove = 8,
    UserState = 9,
    TextMessage = 11,
    PermissionDenied = 12,
    CryptSetup = 15,
    CodecVersion = 21,
    ServerConfig = 24,
    PluginData = 26,
}

impl MessageType {
    pub fn from_u16(v: u16) -> Option<Self> {
        use MessageType::*;
        Some(match v {
            0 => Version,
            1 => UdpTunnel,
            2 => Authenticate,
            3 => Ping,
            4 => Reject,
            5 => ServerSync,
            6 => ChannelRemove,
            7 => ChannelState,
            8 => UserRemove,
            9 => UserState,
            11 => TextMessage,
            12 => PermissionDenied,
            15 => CryptSetup,
            21 => CodecVersion,
            24 => ServerConfig,
            26 => PluginData,
            _ => return None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds limit")]
    TooLarge(usize),
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, ty: u16, payload: &[u8]) -> Result<(), WireError> {
    if payload.len() > MAX_FRAME {
        return Err(WireError::TooLarge(payload.len()));
    }
    let mut buf = BytesMut::with_capacity(6 + payload.len());
    buf.extend_from_slice(&ty.to_be_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<(u16, Bytes), WireError> {
    let mut head = [0u8; 6];
    r.read_exact(&mut head).await?;
    let ty = u16::from_be_bytes([head[0], head[1]]);
    let len = u32::from_be_bytes([head[2], head[3], head[4], head[5]]) as usize;
    if len > MAX_FRAME {
        return Err(WireError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok((ty, Bytes::from(payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 3, b"abc").await.unwrap();
        assert_eq!(buf, [0, 3, 0, 0, 0, 3, b'a', b'b', b'c']);
        let (ty, p) = read_frame(&mut &buf[..]).await.unwrap();
        assert_eq!((ty, &p[..]), (3, &b"abc"[..]));
    }

    #[tokio::test]
    async fn rejects_oversize() {
        let head = [0u8, 1, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(matches!(read_frame(&mut &head[..]).await, Err(WireError::TooLarge(_))));
    }
}
