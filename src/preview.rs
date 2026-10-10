use std::convert::Infallible;
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::stream::{self, Stream};
use tokio::sync::broadcast;

use crate::engine::OutFrame;
use crate::media::{Frame, Kind};

const SET_DATA_FRAME: &[u8] = b"\x02\x00\x0d@setDataFrame";

pub struct Preview {
    tx: broadcast::Sender<OutFrame>,
    headers: Mutex<[Option<Bytes>; 3]>,
}

impl Default for Preview {
    fn default() -> Self {
        Preview { tx: broadcast::channel(2048).0, headers: Mutex::new([None, None, None]) }
    }
}

impl Preview {
    pub fn tap(&self, f: &OutFrame) {
        if is_header(f) {
            self.headers.lock().unwrap()[f.kind.idx()] = Some(f.data.clone());
        }
        if self.tx.receiver_count() > 0 {
            let _ = self.tx.send(f.clone());
        }
    }

    pub fn flv(&self) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + use<> {
        let state = Viewer {
            rx: self.tx.subscribe(),
            headers: self.headers.lock().unwrap().clone(),
            base: None,
            synced: false,
            sent_header: false,
        };
        stream::unfold(state, |mut v| async move {
            if !v.sent_header {
                v.sent_header = true;
                let header = Bytes::from_static(b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00");
                return Some((Ok(header), v));
            }
            loop {
                let f = match v.rx.recv().await {
                    Ok(f) => f,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        v.synced = false;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                if is_header(&f) {
                    v.headers[f.kind.idx()] = Some(f.data.clone());
                    if !v.synced {
                        continue;
                    }
                }
                let mut out = Vec::new();
                if !v.synced {
                    if f.kind != Kind::Video || !Frame::video(f.ts, f.data.clone()).keyframe {
                        continue;
                    }
                    v.synced = true;
                    let base = *v.base.get_or_insert(f.ts);
                    let ts = f.ts.saturating_sub(base);
                    for kind in [Kind::Data, Kind::Video, Kind::Audio] {
                        if let Some(data) = &v.headers[kind.idx()] {
                            out.extend_from_slice(&tag(kind, ts, data));
                        }
                    }
                }
                let base = v.base.unwrap_or(0);
                out.extend_from_slice(&tag(f.kind, f.ts.saturating_sub(base), &f.data));
                return Some((Ok(Bytes::from(out)), v));
            }
        })
    }
}

struct Viewer {
    rx: broadcast::Receiver<OutFrame>,
    headers: [Option<Bytes>; 3],
    base: Option<u32>,
    synced: bool,
    sent_header: bool,
}

fn is_header(f: &OutFrame) -> bool {
    match f.kind {
        Kind::Video => Frame::video(f.ts, f.data.clone()).header,
        Kind::Audio => Frame::audio(f.ts, f.data.clone()).header,
        Kind::Data => f.data.starts_with(SET_DATA_FRAME),
    }
}

fn tag(kind: Kind, ts: u32, data: &[u8]) -> Vec<u8> {
    let data = match kind {
        Kind::Data => data.strip_prefix(SET_DATA_FRAME).unwrap_or(data),
        _ => data,
    };
    let len = data.len() as u32;
    let mut v = Vec::with_capacity(11 + data.len() + 4);
    v.push(kind.type_id());
    v.extend_from_slice(&len.to_be_bytes()[1..]);
    v.extend_from_slice(&ts.to_be_bytes()[1..]);
    v.push((ts >> 24) as u8);
    v.extend_from_slice(&[0, 0, 0]);
    v.extend_from_slice(data);
    v.extend_from_slice(&(11 + len).to_be_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn frame(kind: Kind, ts: u32, data: &'static [u8]) -> OutFrame {
        OutFrame { kind, ts, data: Bytes::from_static(data) }
    }

    #[tokio::test]
    async fn late_joiner_gets_headers_then_keyframe() {
        let p = Preview::default();
        p.tap(&frame(Kind::Data, 0, b"\x02\x00\x0d@setDataFrame\x02\x00\x0aonMetaData"));
        p.tap(&frame(Kind::Video, 0, &[0x17, 0x00, 0, 0, 0, 1]));
        p.tap(&frame(Kind::Audio, 0, &[0xaf, 0x00, 0x12, 0x10]));
        let mut s = Box::pin(p.flv());
        assert_eq!(&s.next().await.unwrap().unwrap()[..3], b"FLV");

        p.tap(&frame(Kind::Video, 4000, &[0x27, 0x01, 0, 0, 0]));
        p.tap(&frame(Kind::Video, 4040, &[0x17, 0x01, 0, 0, 0]));
        let chunk = s.next().await.unwrap().unwrap();
        let types: Vec<u8> = tags(&chunk).iter().map(|t| t.0).collect();
        assert_eq!(types, vec![18, 9, 8, 9]);
        assert!(tags(&chunk).iter().all(|t| t.1 == 0));
        assert_eq!(&chunk[11..14], b"\x02\x00\x0a", "onMetaData without @setDataFrame");

        p.tap(&frame(Kind::Audio, 4060, &[0xaf, 0x01, 0x21]));
        let chunk = s.next().await.unwrap().unwrap();
        assert_eq!(tags(&chunk), vec![(8, 20)]);
    }

    fn tags(b: &[u8]) -> Vec<(u8, u32)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let len = u32::from_be_bytes([0, b[i + 1], b[i + 2], b[i + 3]]) as usize;
            let ts = u32::from_be_bytes([b[i + 7], b[i + 4], b[i + 5], b[i + 6]]);
            out.push((b[i], ts));
            i += 11 + len + 4;
        }
        out
    }
}
