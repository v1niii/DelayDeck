use std::time::{Duration, Instant};

use bytes::Bytes;
use delaydeck::engine::OutFrame;
use delaydeck::media::Kind;
use delaydeck::output::{Publisher, parse_url};
use rml_rtmp::rml_amf0::{self, Amf0Value};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "rtmp://127.0.0.1:1935/live".into());
    let key = args.next().unwrap_or_else(|| "fake-key".into());
    let seconds: f64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(3600.0);

    let mut obs = Publisher::connect(&parse_url(&url)?, &key).await?;
    eprintln!("publishing to {url} for {seconds}s (Ctrl+C to stop)");

    let mut props = std::collections::HashMap::new();
    for (k, v) in [("width", 1920.0), ("height", 1080.0), ("framerate", 30.0), ("videocodecid", 7.0), ("audiocodecid", 10.0)] {
        props.insert(k.to_string(), Amf0Value::Number(v));
    }
    props.insert("encoder".into(), Amf0Value::Utf8String("DelayDeck fake_obs".into()));
    let meta = rml_amf0::serialize(&vec![
        Amf0Value::Utf8String("@setDataFrame".into()),
        Amf0Value::Utf8String("onMetaData".into()),
        Amf0Value::Object(props),
    ])?;
    obs.queue(OutFrame { kind: Kind::Data, ts: 0, data: Bytes::from(meta) })?;
    obs.queue(OutFrame { kind: Kind::Video, ts: 0, data: Bytes::from_static(&[0x17, 0, 0, 0, 0, 1, 0x64, 0, 0x28, 0xff]) })?;
    obs.queue(OutFrame { kind: Kind::Audio, ts: 0, data: Bytes::from_static(&[0xaf, 0x00, 0x11, 0x90]) })?;

    let start = Instant::now();
    let (mut frame, mut audio_ms) = (0u64, 0u64);
    while start.elapsed().as_secs_f64() < seconds {
        let now_ms = start.elapsed().as_millis() as u64;
        while frame * 1000 / 30 <= now_ms {
            let key_frame = frame % 60 == 0;
            let size = if key_frame { 60_000 } else { 9_000 };
            let mut data = vec![0u8; size];
            data[0] = if key_frame { 0x17 } else { 0x27 };
            data[1] = 1;
            obs.queue(OutFrame { kind: Kind::Video, ts: (frame * 1000 / 30) as u32, data: Bytes::from(data) })?;
            frame += 1;
        }
        while audio_ms <= now_ms {
            let mut data = vec![0u8; 400];
            data[0] = 0xaf;
            data[1] = 1;
            obs.queue(OutFrame { kind: Kind::Audio, ts: audio_ms as u32, data: Bytes::from(data) })?;
            audio_ms += 21;
        }
        obs.flush().await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    obs.finish().await;
    Ok(())
}
