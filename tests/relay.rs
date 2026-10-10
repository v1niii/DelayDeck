use std::time::{Duration, Instant};

use bytes::Bytes;
use delaydeck::config::{Config, Service};
use delaydeck::engine::OutFrame;
use delaydeck::media::Kind;
use delaydeck::output::{Publisher, parse_url};
use delaydeck::rtmp::{Conn, Incoming, n, obj, s};
use delaydeck::state::{self, App};
use delaydeck::{ingest, output};
use rml_rtmp::rml_amf0::{self, Amf0Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::sleep;

#[derive(Debug)]
enum Seen {
    Key(String),
    Meta,
    VideoHeader,
    AudioHeader,
    Video { at: Instant, ts: u32, id: u32 },
    Closed,
}

async fn platform(listener: TcpListener, tx: mpsc::UnboundedSender<Seen>) {
    let (sock, _) = listener.accept().await.unwrap();
    let mut conn = Conn::accept(sock).await.unwrap();
    loop {
        let Ok(msgs) = conn.recv().await else {
            let _ = tx.send(Seen::Closed);
            return;
        };
        for msg in msgs {
            let seen = match msg {
                Incoming::Command { name, tid, args, .. } => {
                    match name.as_str() {
                        "connect" => {
                            let info = obj(&[("level", s("status")), ("code", s("NetConnection.Connect.Success"))]);
                            conn.command("_result", tid, obj(&[]), vec![info], 0).unwrap();
                        }
                        "createStream" => conn.command("_result", tid, Amf0Value::Null, vec![n(1.0)], 0).unwrap(),
                        "publish" => {
                            let info = obj(&[("level", s("status")), ("code", s("NetStream.Publish.Start"))]);
                            conn.command("onStatus", 0.0, Amf0Value::Null, vec![info], 1).unwrap();
                            if let Some(Amf0Value::Utf8String(key)) = args.first() {
                                let _ = tx.send(Seen::Key(key.clone()));
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                Incoming::Data { .. } => Seen::Meta,
                Incoming::Audio { data, .. } if data[1] == 0 => Seen::AudioHeader,
                Incoming::Audio { .. } => continue,
                Incoming::Video { data, .. } if data[1] == 0 => Seen::VideoHeader,
                Incoming::Video { ts, data } => {
                    let id = u32::from_be_bytes(data[5..9].try_into().unwrap());
                    Seen::Video { at: Instant::now(), ts, id }
                }
            };
            let _ = tx.send(seen);
        }
        conn.flush().await.unwrap();
    }
}

fn video(id: u32, key: bool) -> OutFrame {
    let mut d = vec![if key { 0x17 } else { 0x27 }, 0x01, 0, 0, 0];
    d.extend_from_slice(&id.to_be_bytes());
    OutFrame { kind: Kind::Video, ts: id * 40, data: Bytes::from(d) }
}

fn metadata() -> OutFrame {
    let mut props = std::collections::HashMap::new();
    props.insert("width".to_string(), Amf0Value::Number(1280.0));
    props.insert("height".to_string(), Amf0Value::Number(720.0));
    props.insert("framerate".to_string(), Amf0Value::Number(25.0));
    let values = vec![s("@setDataFrame"), s("onMetaData"), Amf0Value::Object(props)];
    OutFrame { kind: Kind::Data, ts: 0, data: Bytes::from(rml_amf0::serialize(&values).unwrap()) }
}

#[tokio::test]
async fn relays_with_delay_skip_replay_and_drain() {
    let platform_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let platform_port = platform_listener.local_addr().unwrap().port();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(platform(platform_listener, tx));

    let config = Config {
        service: Service::Custom,
        custom_url: format!("rtmp://127.0.0.1:{platform_port}/app"),
        delay: 3.0,
        ..Config::default()
    };
    let config_path = std::env::temp_dir().join(format!("delaydeck-test-{}.json", std::process::id()));
    let app = App::new(config, config_path.clone());
    let ingest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ingest_port = ingest_listener.local_addr().unwrap().port();
    tokio::spawn(ingest::run(app.clone(), ingest_listener));
    tokio::spawn(output::run(app.clone()));
    tokio::spawn(state::status_loop(app.clone()));

    let obs_url = parse_url(&format!("rtmp://127.0.0.1:{ingest_port}/live")).unwrap();
    let mut obs = Publisher::connect(&obs_url, "live_test_key").await.expect("OBS publish accepted");
    obs.queue(metadata()).unwrap();
    obs.queue(OutFrame { kind: Kind::Video, ts: 0, data: Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f]) }).unwrap();
    obs.queue(OutFrame { kind: Kind::Audio, ts: 0, data: Bytes::from_static(&[0xaf, 0x00, 0x12, 0x10]) }).unwrap();
    let started = Instant::now();
    let mut id = 0u32;
    let mut send_until = |obs: &mut Publisher, secs: f64| {
        let mut frames = Vec::new();
        while (id as f64) * 0.04 < secs {
            frames.push(video(id, id.is_multiple_of(25)));
            id += 1;
        }
        for f in frames {
            obs.queue(f).unwrap();
        }
    };
    async fn pace(obs: &mut Publisher, started: Instant, until: f64, send: &mut impl FnMut(&mut Publisher, f64)) {
        while started.elapsed().as_secs_f64() < until {
            send(obs, started.elapsed().as_secs_f64());
            obs.flush().await.unwrap();
            sleep(Duration::from_millis(20)).await;
        }
    }

    pace(&mut obs, started, 5.0, &mut send_until).await;
    app.set_delay(0.0);
    pace(&mut obs, started, 6.5, &mut send_until).await;
    app.set_delay(2.0);
    pace(&mut obs, started, 10.5, &mut send_until).await;
    obs.finish().await;

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(Seen::Closed)) => break,
            Ok(Some(s)) => seen.push(s),
            Ok(None) => break,
            Err(_) => {}
        }
    }
    let _ = std::fs::remove_file(&config_path);

    assert!(matches!(seen.first(), Some(Seen::Key(k)) if k == "live_test_key"), "stream key passed through: {:?}", seen.first());
    let first_media = seen.iter().position(|s| matches!(s, Seen::Video { .. })).expect("video arrived");
    assert!(seen[..first_media].iter().any(|s| matches!(s, Seen::Meta)), "metadata before media");
    assert!(seen[..first_media].iter().any(|s| matches!(s, Seen::VideoHeader)), "AVC config before media");
    assert!(seen[..first_media].iter().any(|s| matches!(s, Seen::AudioHeader)), "AAC config before media");
    let headers = seen[..first_media].iter().filter(|s| matches!(s, Seen::Meta | Seen::VideoHeader | Seen::AudioHeader)).count();
    assert_eq!(headers, 3, "each header sent once at stream start");

    let frames: Vec<(Instant, u32, u32)> = seen
        .iter()
        .filter_map(|s| match s {
            Seen::Video { at, ts, id } => Some((*at, *ts, *id)),
            _ => None,
        })
        .collect();
    let lag = |&(at, _, id): &(Instant, u32, u32)| at.duration_since(started).as_secs_f64() - id as f64 * 0.04;

    assert_eq!(frames[0].2, 0, "starts on the first keyframe");
    assert!((2.9..3.6).contains(&lag(&frames[0])), "initial delay ~3s, got {:.2}", lag(&frames[0]));
    assert!(frames.windows(2).all(|w| w[1].1 >= w[0].1), "outgoing timestamps never go backwards");

    let skip = frames.windows(2).find(|w| w[1].2 > w[0].2 + 1).expect("skip to live dropped frames");
    assert_eq!(skip[1].2 % 25, 0, "skip landed on a keyframe");
    let after_skip = frames.iter().find(|f| f.0.duration_since(started).as_secs_f64() > 5.5).unwrap();
    assert!(lag(after_skip) < 0.3, "live after skip, lag {:.2}", lag(after_skip));

    let replay = frames.windows(2).find(|w| w[1].2 < w[0].2).expect("replay jumped back");
    assert_eq!(replay[1].2 % 25, 0, "replay landed on a keyframe");
    let last = frames.last().unwrap();
    assert!((1.8..2.6).contains(&lag(last)), "settled at ~2s, got {:.2}", lag(last));
    assert_eq!(last.2, id - 1, "drained every frame after OBS stopped");
}
