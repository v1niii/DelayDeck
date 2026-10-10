use std::collections::HashMap;

use bytes::Bytes;
use rml_rtmp::chunk_io::{ChunkDeserializer, ChunkSerializer};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::messages::{MessagePayload, RtmpMessage, UserControlEventType};
use rml_rtmp::rml_amf0::Amf0Value;
use rml_rtmp::time::RtmpTimestamp;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub enum Incoming {
    Command { name: String, tid: f64, object: Amf0Value, args: Vec<Amf0Value>, stream_id: u32 },
    Data { ts: u32, data: Bytes },
    Audio { ts: u32, data: Bytes },
    Video { ts: u32, data: Bytes },
}

pub struct Conn<S> {
    io: S,
    ser: ChunkSerializer,
    de: ChunkDeserializer,
    rbuf: Vec<u8>,
    leftover: Vec<u8>,
    wbuf: Vec<u8>,
    received: u64,
    ack_window: u64,
    acked: u64,
}

impl<S: Stream> Conn<S> {
    pub async fn accept(io: S) -> Result<Self> {
        Self::handshake(io, PeerType::Server).await
    }

    pub async fn open(io: S) -> Result<Self> {
        Self::handshake(io, PeerType::Client).await
    }

    async fn handshake(mut io: S, peer: PeerType) -> Result<Self> {
        let client = peer == PeerType::Client;
        let mut hs = Handshake::new(peer);
        if client {
            let p0p1 = hs.generate_outbound_p0_and_p1()?;
            io.write_all(&p0p1).await?;
            io.flush().await?;
        }
        let mut buf = vec![0u8; 8192];
        loop {
            let n = io.read(&mut buf).await?;
            if n == 0 {
                return Err(closed());
            }
            match hs.process_bytes(&buf[..n])? {
                HandshakeProcessResult::InProgress { response_bytes } => {
                    if !response_bytes.is_empty() {
                        io.write_all(&response_bytes).await?;
                        io.flush().await?;
                    }
                }
                HandshakeProcessResult::Completed { response_bytes, remaining_bytes } => {
                    if !response_bytes.is_empty() {
                        io.write_all(&response_bytes).await?;
                        io.flush().await?;
                    }
                    return Ok(Conn {
                        io,
                        ser: ChunkSerializer::new(),
                        de: ChunkDeserializer::new(),
                        rbuf: vec![0; 64 * 1024],
                        leftover: remaining_bytes,
                        wbuf: Vec::new(),
                        received: 0,
                        ack_window: 0,
                        acked: 0,
                    });
                }
            }
        }
    }

    pub async fn recv(&mut self) -> Result<Vec<Incoming>> {
        if !self.leftover.is_empty() {
            let bytes = std::mem::take(&mut self.leftover);
            return self.decode(&bytes);
        }
        let n = self.io.read(&mut self.rbuf).await?;
        if n == 0 {
            return Err(closed());
        }
        let buf = std::mem::take(&mut self.rbuf);
        let result = self.decode(&buf[..n]);
        self.rbuf = buf;
        result
    }

    fn decode(&mut self, bytes: &[u8]) -> Result<Vec<Incoming>> {
        self.received += bytes.len() as u64;
        let mut out = Vec::new();
        let mut input = bytes;
        while let Some(payload) = self.de.get_next_message(input)? {
            input = &[];
            let ts = payload.timestamp.value;
            match payload.type_id {
                8 => out.push(Incoming::Audio { ts, data: payload.data }),
                9 => out.push(Incoming::Video { ts, data: payload.data }),
                18 => out.push(Incoming::Data { ts, data: payload.data }),
                _ => {
                    let stream_id = payload.message_stream_id;
                    match payload.to_rtmp_message()? {
                        RtmpMessage::SetChunkSize { size } => self.de.set_max_chunk_size(size as usize)?,
                        RtmpMessage::WindowAcknowledgement { size } => self.ack_window = size as u64,
                        RtmpMessage::UserControl { event_type: UserControlEventType::PingRequest, timestamp, .. } => {
                            let pong = RtmpMessage::UserControl {
                                event_type: UserControlEventType::PingResponse,
                                stream_id: None,
                                buffer_length: None,
                                timestamp,
                            };
                            self.queue(pong, 0, 0)?;
                        }
                        RtmpMessage::Amf0Command { command_name, transaction_id, command_object, additional_arguments } => {
                            out.push(Incoming::Command {
                                name: command_name,
                                tid: transaction_id,
                                object: command_object,
                                args: additional_arguments,
                                stream_id,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        if self.ack_window > 0 && self.received - self.acked >= self.ack_window {
            self.acked = self.received;
            self.queue(RtmpMessage::Acknowledgement { sequence_number: self.received as u32 }, 0, 0)?;
        }
        Ok(out)
    }

    pub fn queue(&mut self, msg: RtmpMessage, ts: u32, stream_id: u32) -> Result<()> {
        let payload = msg.into_message_payload(RtmpTimestamp::new(ts), stream_id)?;
        self.queue_payload(&payload)
    }

    fn queue_payload(&mut self, payload: &MessagePayload) -> Result<()> {
        let packet = self.ser.serialize(payload, false, false)?;
        self.wbuf.extend_from_slice(&packet.bytes);
        Ok(())
    }

    pub fn queue_media(&mut self, type_id: u8, ts: u32, stream_id: u32, data: Bytes) -> Result<()> {
        self.queue_payload(&MessagePayload { timestamp: RtmpTimestamp::new(ts), message_stream_id: stream_id, type_id, data })
    }

    pub fn command(&mut self, name: &str, tid: f64, object: Amf0Value, args: Vec<Amf0Value>, stream_id: u32) -> Result<()> {
        let msg = RtmpMessage::Amf0Command {
            command_name: name.to_string(),
            transaction_id: tid,
            command_object: object,
            additional_arguments: args,
        };
        self.queue(msg, 0, stream_id)
    }

    pub fn set_chunk_size(&mut self, size: u32) -> Result<()> {
        let packet = self.ser.set_max_chunk_size(size, RtmpTimestamp::new(0))?;
        self.wbuf.extend_from_slice(&packet.bytes);
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<usize> {
        if self.wbuf.is_empty() {
            return Ok(0);
        }
        self.io.write_all(&self.wbuf).await?;
        self.io.flush().await?;
        let n = self.wbuf.len();
        self.wbuf.clear();
        if self.wbuf.capacity() > 8 << 20 {
            self.wbuf.shrink_to(256 << 10);
        }
        Ok(n)
    }

    pub async fn shutdown(&mut self) {
        let _ = self.io.shutdown().await;
    }
}

fn closed() -> Error {
    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed by peer").into()
}

pub fn is_closed(e: &Error) -> bool {
    use std::io::ErrorKind::*;
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| matches!(e.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe))
}

pub fn obj(props: &[(&str, Amf0Value)]) -> Amf0Value {
    Amf0Value::Object(props.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<HashMap<_, _>>())
}

pub fn s(v: &str) -> Amf0Value {
    Amf0Value::Utf8String(v.to_string())
}

pub fn n(v: f64) -> Amf0Value {
    Amf0Value::Number(v)
}

pub fn prop<'a>(object: &'a Amf0Value, key: &str) -> Option<&'a str> {
    match object {
        Amf0Value::Object(p) => match p.get(key) {
            Some(Amf0Value::Utf8String(v)) => Some(v),
            _ => None,
        },
        _ => None,
    }
}
