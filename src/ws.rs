//! WebSocket endpoint for real-time board updates and bi-directional sync.

use crate::events::BoardEvent;
use crate::store::{CatchUpResult, SharedBoard};

use axum::extract::{Request, State as AxState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures::stream::StreamExt;
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_stream::wrappers::BroadcastStream;

#[derive(Debug, PartialEq, Eq)]
pub enum WsFrame {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Subscribe { last_seq: Option<u64> },
    Ping,
    Pong,
}

/// Computes SHA-1 hash (RFC 3174) for pure Rust WebSocket key verification.
#[allow(clippy::needless_range_loop)]
fn sha1(input: &[u8]) -> [u8; 20] {
    let mut h = [
        0x67452301u32,
        0xEFCDAB89u32,
        0x98BADCFEu32,
        0x10325476u32,
        0xC3D2E1F0u32,
    ];
    let bit_len = (input.len() as u64) * 8;
    let mut msg = input.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let (blocks, _tail) = msg.as_chunks::<64>();
    for chunk in blocks {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for i in 0..5 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

pub fn compute_ws_accept(key: &str) -> String {
    let concatenated = format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let digest = sha1(concatenated.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Option<WsFrame>> {
    let mut header = [0u8; 2];
    if reader.read_exact(&mut header).await.is_err() {
        return Ok(None);
    }
    let _fin = (header[0] & 0x80) != 0;
    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let mut len = (header[1] & 0x7F) as u64;

    if len == 126 {
        let mut len_bytes = [0u8; 2];
        reader.read_exact(&mut len_bytes).await?;
        len = u16::from_be_bytes(len_bytes) as u64;
    } else if len == 127 {
        let mut len_bytes = [0u8; 8];
        reader.read_exact(&mut len_bytes).await?;
        len = u64::from_be_bytes(len_bytes);
    }

    if len > 10 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }

    let mask = if masked {
        let mut mask_bytes = [0u8; 4];
        reader.read_exact(&mut mask_bytes).await?;
        Some(mask_bytes)
    } else {
        None
    };

    let mut payload = vec![0u8; len as usize];
    if len > 0 {
        reader.read_exact(&mut payload).await?;
    }

    if let Some(mask_bytes) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask_bytes[i % 4];
        }
    }

    match opcode {
        0x1 => {
            let text = String::from_utf8(payload)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(Some(WsFrame::Text(text)))
        }
        0x2 => Ok(Some(WsFrame::Binary(payload))),
        0x8 => Ok(Some(WsFrame::Close)),
        0x9 => Ok(Some(WsFrame::Ping(payload))),
        0xA => Ok(Some(WsFrame::Pong(payload))),
        _ => Ok(Some(WsFrame::Pong(vec![]))),
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: WsFrame,
) -> std::io::Result<()> {
    let (opcode, payload) = match frame {
        WsFrame::Text(text) => (0x81u8, text.into_bytes()),
        WsFrame::Binary(data) => (0x82u8, data),
        WsFrame::Ping(data) => (0x89u8, data),
        WsFrame::Pong(data) => (0x8Au8, data),
        WsFrame::Close => (0x88u8, vec![]),
    };

    let mut header = vec![opcode];
    let len = payload.len();
    if len < 126 {
        header.push(len as u8);
    } else if len <= 65535 {
        header.push(126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }

    writer.write_all(&header).await?;
    if !payload.is_empty() {
        writer.write_all(&payload).await?;
    }
    writer.flush().await?;
    Ok(())
}

pub async fn handle_ws_connection<S>(stream: S, b: SharedBoard)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WsFrame>(32);

    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if write_frame(&mut writer, frame).await.is_err() {
                break;
            }
        }
    });

    let mut subscribed = false;
    let mut min_live_seq = 0;

    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut live_rx = BroadcastStream::new(b.subscribe());

    loop {
        tokio::select! {
            frame_res = read_frame(&mut reader) => {
                match frame_res {
                    Ok(Some(WsFrame::Text(text))) => {
                        if let Ok(msg) = serde_json::from_str::<ClientMessage>(&text) {
                            match msg {
                                ClientMessage::Subscribe { last_seq } => {
                                    let (initial_events, max_seq) = match last_seq {
                                        Some(seq) => match b.catch_up(seq) {
                                            CatchUpResult::Events(missed) => {
                                                let max_seq = missed.last().map(|e| e.seq()).unwrap_or(seq);
                                                (missed, max_seq)
                                            }
                                            CatchUpResult::Reset { seq: current_seq } => {
                                                (vec![BoardEvent::Reset { seq: current_seq }], current_seq)
                                            }
                                        },
                                        None => (vec![], 0),
                                    };

                                    min_live_seq = max_seq;
                                    for ev in initial_events {
                                        if let Ok(json) = serde_json::to_string(&ev) {
                                            if tx.send(WsFrame::Text(json)).await.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                    subscribed = true;
                                }
                                ClientMessage::Ping => {
                                    let _ = tx.send(WsFrame::Text(r#"{"type":"pong"}"#.to_string())).await;
                                }
                                ClientMessage::Pong => {}
                            }
                        }
                    }
                    Ok(Some(WsFrame::Binary(_))) => {
                        // Board sync is text-only; ignore binary frames.
                    }
                    Ok(Some(WsFrame::Ping(data))) => {
                        if tx.send(WsFrame::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Some(WsFrame::Pong(_))) => {}
                    Ok(Some(WsFrame::Close)) | Ok(None) | Err(_) => {
                        break;
                    }
                }
            }
            _ = ping_interval.tick() => {
                if tx.send(WsFrame::Ping(vec![])).await.is_err() {
                    break;
                }
            }
            msg = live_rx.next(), if subscribed => {
                match msg {
                    Some(Ok(ev)) => {
                        if ev.seq() > min_live_seq {
                            if let Ok(json) = serde_json::to_string(&ev) {
                                if tx.send(WsFrame::Text(json)).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Err(_lagged)) => {
                        let reset_ev = BoardEvent::Reset { seq: b.current_seq() };
                        if let Ok(json) = serde_json::to_string(&reset_ev) {
                            if tx.send(WsFrame::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }

    drop(tx);
    let _ = writer_task.await;
}

pub async fn ws_handler(
    AxState(b): AxState<SharedBoard>,
    headers: HeaderMap,
    mut req: Request,
) -> Response {
    let key = match headers.get("sec-websocket-key").and_then(|v| v.to_str().ok()) {
        Some(k) => k,
        None => return (StatusCode::BAD_REQUEST, "Missing Sec-WebSocket-Key").into_response(),
    };

    let accept_key = compute_ws_accept(key);

    let on_upgrade = hyper::upgrade::on(&mut req);

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let io = hyper_util::rt::TokioIo::new(upgraded);
                handle_ws_connection(io, b).await;
            }
            Err(e) => {
                tracing::debug!("ws upgrade error: {e}");
            }
        }
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header("sec-websocket-accept", accept_key)
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "Response build error").into_response()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Origin;
    use crate::schema::Schema;
    use crate::store::Board;
    use std::sync::Arc;

    #[test]
    fn test_ws_accept_key_rfc6455_example() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = compute_ws_accept(key);
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[tokio::test]
    async fn test_ws_read_write_frame_roundtrip() {
        let (mut client_io, mut server_io) = tokio::io::duplex(1024);

        let frame = WsFrame::Text(r#"{"type":"subscribe","last_seq":5}"#.to_string());
        write_frame(&mut client_io, frame).await.unwrap();

        let read = read_frame(&mut server_io).await.unwrap();
        assert_eq!(
            read,
            Some(WsFrame::Text(
                r#"{"type":"subscribe","last_seq":5}"#.to_string()
            ))
        );
    }

    #[tokio::test]
    async fn test_ws_connection_handler_subscribe_and_catchup() {
        let b = Arc::new(
            Board::new(
                Schema::default(),
                std::env::temp_dir().join(format!("sandboard-ws-test-{}.json", std::process::id())),
            )
            .with_buffer_capacity(10),
        );

        let _p = b
            .create(None, "WS Proj", "intent", None, Origin::Human, true, None)
            .unwrap();
        let cur_seq = b.current_seq();

        let (client_io, server_io) = tokio::io::duplex(4096);
        let b_clone = b.clone();
        tokio::spawn(async move {
            handle_ws_connection(server_io, b_clone).await;
        });

        let (mut c_read, mut c_write) = tokio::io::split(client_io);

        // Send subscribe message with last_seq = cur_seq - 1
        let sub_msg = format!(r#"{{"type":"subscribe","last_seq":{}}}"#, cur_seq - 1);
        write_frame(&mut c_write, WsFrame::Text(sub_msg))
            .await
            .unwrap();

        // Read replayed event
        let frame = read_frame(&mut c_read).await.unwrap();
        assert!(frame.is_some());
        if let Some(WsFrame::Text(json)) = frame {
            assert!(json.contains("WS Proj"));
        } else {
            panic!("Expected text frame");
        }

        // Send ping message
        write_frame(&mut c_write, WsFrame::Text(r#"{"type":"ping"}"#.to_string()))
            .await
            .unwrap();

        let pong_frame = read_frame(&mut c_read).await.unwrap();
        assert_eq!(pong_frame, Some(WsFrame::Text(r#"{"type":"pong"}"#.to_string())));
    }
}
