use std::io::Cursor;

use bytes::Bytes;
use rml_rtmp::rml_amf0::{self, Amf0Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Video,
    Data,
}

impl Kind {
    pub fn type_id(self) -> u8 {
        match self {
            Kind::Audio => 8,
            Kind::Video => 9,
            Kind::Data => 18,
        }
    }

    pub fn idx(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub kind: Kind,
    pub ts: u32,
    pub data: Bytes,
    pub keyframe: bool,
    pub header: bool,
}

impl Frame {
    pub fn video(ts: u32, data: Bytes) -> Frame {
        let (keyframe, header) = inspect_video(&data);
        Frame { kind: Kind::Video, ts, data, keyframe, header }
    }

    pub fn audio(ts: u32, data: Bytes) -> Frame {
        let header = is_audio_header(&data);
        Frame { kind: Kind::Audio, ts, data, keyframe: false, header }
    }

    pub fn data(ts: u32, data: Bytes, metadata: bool) -> Frame {
        Frame { kind: Kind::Data, ts, data, keyframe: false, header: metadata }
    }
}

fn inspect_video(d: &[u8]) -> (bool, bool) {
    let Some(&b0) = d.first() else { return (false, false) };
    if b0 & 0x80 != 0 {
        let frame_type = (b0 >> 4) & 0x07;
        let mut packet_type = b0 & 0x0f;
        if packet_type == 6 {
            packet_type = d.get(1).map_or(1, |b| b & 0x0f);
        }
        let header = matches!(packet_type, 0 | 5);
        (frame_type == 1 && matches!(packet_type, 1 | 3), header)
    } else {
        let header = matches!(b0 & 0x0f, 7 | 12) && d.get(1) == Some(&0);
        (b0 >> 4 == 1 && !header, header)
    }
}

fn is_audio_header(d: &[u8]) -> bool {
    let Some(&b0) = d.first() else { return false };
    match b0 >> 4 {
        10 => d.get(1) == Some(&0),
        9 => {
            let mut packet_type = b0 & 0x0f;
            if packet_type == 5 {
                packet_type = d.get(1).map_or(1, |b| b & 0x0f);
            }
            matches!(packet_type, 0 | 4)
        }
        _ => false,
    }
}

pub fn video_codec(d: &[u8]) -> Option<&'static str> {
    let &b0 = d.first()?;
    if b0 & 0x80 != 0 {
        let at = if b0 & 0x0f == 6 { 2 } else { 1 };
        fourcc_name(d.get(at..at + 4)?)
    } else {
        match b0 & 0x0f {
            7 => Some("H.264"),
            12 => Some("HEVC"),
            2 => Some("H.263"),
            4 | 5 => Some("VP6"),
            _ => None,
        }
    }
}

pub fn audio_codec(d: &[u8]) -> Option<&'static str> {
    let &b0 = d.first()?;
    match b0 >> 4 {
        10 => Some("AAC"),
        2 | 14 => Some("MP3"),
        11 => Some("Speex"),
        0 | 3 => Some("PCM"),
        9 => {
            let at = if b0 & 0x0f == 5 { 2 } else { 1 };
            fourcc_name(d.get(at..at + 4)?)
        }
        _ => None,
    }
}

fn fourcc_name(cc: &[u8]) -> Option<&'static str> {
    Some(match cc {
        b"avc1" => "H.264",
        b"hvc1" => "HEVC",
        b"av01" => "AV1",
        b"vp09" => "VP9",
        b"vp08" => "VP8",
        b"mp4a" => "AAC",
        b"Opus" => "Opus",
        b"fLaC" => "FLAC",
        b"ac-3" => "AC-3",
        b"ec-3" => "E-AC-3",
        b".mp3" => "MP3",
        _ => return None,
    })
}

#[derive(Clone, Debug, Default)]
pub struct MetaInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub encoder: Option<String>,
}

pub fn parse_data(raw: Bytes) -> (Bytes, Option<MetaInfo>) {
    let Ok(values) = rml_amf0::deserialize(&mut Cursor::new(&raw[..])) else { return (raw, None) };
    let name = |i: usize| match values.get(i) {
        Some(Amf0Value::Utf8String(s)) => s.as_str(),
        _ => "",
    };
    if name(0) == "@setDataFrame" && name(1) == "onMetaData" {
        let info = meta_info(values.get(2));
        (raw, Some(info))
    } else if name(0) == "onMetaData" {
        let mut v = Vec::with_capacity(raw.len() + 16);
        v.extend_from_slice(&[0x02, 0x00, 13]);
        v.extend_from_slice(b"@setDataFrame");
        v.extend_from_slice(&raw);
        let info = meta_info(values.get(1));
        (Bytes::from(v), Some(info))
    } else {
        (raw, None)
    }
}

fn meta_info(v: Option<&Amf0Value>) -> MetaInfo {
    let Some(Amf0Value::Object(p)) = v else { return MetaInfo::default() };
    let num = |k: &str| match p.get(k) {
        Some(Amf0Value::Number(n)) if *n > 0.0 => Some(*n),
        _ => None,
    };
    MetaInfo {
        width: num("width").map(|n| n as u32),
        height: num("height").map(|n| n as u32),
        fps: num("framerate").or_else(|| num("videoframerate")),
        encoder: match p.get("encoder") {
            Some(Amf0Value::Utf8String(s)) => Some(s.clone()),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_avc() {
        assert_eq!(inspect_video(&[0x17, 0x00, 0, 0, 0]), (false, true));
        assert_eq!(inspect_video(&[0x17, 0x01, 0, 0, 0]), (true, false));
        assert_eq!(inspect_video(&[0x27, 0x01, 0, 0, 0]), (false, false));
        assert_eq!(video_codec(&[0x17, 0x01]), Some("H.264"));
    }

    #[test]
    fn enhanced_hevc() {
        assert_eq!(inspect_video(&[0x90, b'h', b'v', b'c', b'1']), (false, true));
        assert_eq!(inspect_video(&[0x93, b'h', b'v', b'c', b'1']), (true, false));
        assert_eq!(inspect_video(&[0xa1, b'h', b'v', b'c', b'1']), (false, false));
        assert_eq!(video_codec(&[0x93, b'h', b'v', b'c', b'1']), Some("HEVC"));
    }

    #[test]
    fn audio_headers() {
        assert!(is_audio_header(&[0xaf, 0x00, 0x12, 0x10]));
        assert!(!is_audio_header(&[0xaf, 0x01, 0x21]));
        assert!(is_audio_header(&[0x90, b'O', b'p', b'u', b's']));
        assert_eq!(audio_codec(&[0x91, b'O', b'p', b'u', b's']), Some("Opus"));
    }

    #[test]
    fn metadata_is_normalised() {
        let mut props = std::collections::HashMap::new();
        props.insert("width".to_string(), Amf0Value::Number(1920.0));
        props.insert("height".to_string(), Amf0Value::Number(1080.0));
        props.insert("framerate".to_string(), Amf0Value::Number(60.0));
        let bare = rml_amf0::serialize(&vec![Amf0Value::Utf8String("onMetaData".into()), Amf0Value::Object(props)]).unwrap();
        let (data, info) = parse_data(Bytes::from(bare));
        let info = info.expect("metadata detected");
        assert_eq!((info.width, info.height, info.fps), (Some(1920), Some(1080), Some(60.0)));
        let values = rml_amf0::deserialize(&mut Cursor::new(&data[..])).unwrap();
        assert_eq!(values[0], Amf0Value::Utf8String("@setDataFrame".into()));
        assert_eq!(values[1], Amf0Value::Utf8String("onMetaData".into()));
    }
}
