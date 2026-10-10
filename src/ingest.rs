use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use rml_rtmp::messages::{PeerBandwidthLimitType, RtmpMessage, UserControlEventType};
use rml_rtmp::rml_amf0::Amf0Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use crate::media::{self, Frame};
use crate::rtmp::{self, Conn, Incoming, Result, n, obj, s};
use crate::state::{App, Level};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(app: Arc<App>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((sock, addr)) => {
                let _ = sock.set_nodelay(true);
                let app = app.clone();
                tokio::spawn(async move {
                    let mut session = Session::new(app.clone(), addr);
                    let result = session.serve(sock).await;
                    if session.publishing {
                        app.end_publish();
                    }
                    if let Err(e) = result
                        && !rtmp::is_closed(&e) {
                            app.log(Level::Warn, format!("OBS connection ended: {e}"));
                        }
                });
            }
            Err(e) => {
                app.log(Level::Warn, format!("RTMP accept failed: {e}"));
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

struct Session {
    app: Arc<App>,
    addr: SocketAddr,
    app_name: String,
    publishing: bool,
    seen_video: bool,
    seen_audio: bool,
    reject: Option<&'static str>,
}

impl Session {
    fn new(app: Arc<App>, addr: SocketAddr) -> Session {
        Session { app, addr, app_name: String::new(), publishing: false, seen_video: false, seen_audio: false, reject: None }
    }

    async fn serve(&mut self, sock: TcpStream) -> Result<()> {
        let mut conn = timeout(HANDSHAKE_TIMEOUT, Conn::accept(sock)).await.map_err(|_| "RTMP handshake timed out")??;
        loop {
            let msgs = timeout(IDLE_TIMEOUT, conn.recv()).await.map_err(|_| "no data from OBS for 30 seconds")??;
            let now = Instant::now();
            for msg in msgs {
                self.handle(&mut conn, msg, now)?;
            }
            conn.flush().await?;
            if let Some(reason) = self.reject.take() {
                return Err(reason.into());
            }
        }
    }

    fn handle(&mut self, conn: &mut Conn<TcpStream>, msg: Incoming, now: Instant) -> Result<()> {
        match msg {
            Incoming::Command { name, tid, object, args, stream_id } => self.command(conn, &name, tid, &object, &args, stream_id)?,
            Incoming::Video { ts, data } if self.publishing => {
                if !self.seen_video
                    && let Some(codec) = media::video_codec(&data) {
                        self.seen_video = true;
                        self.app.with_input(|i| i.video = Some(codec));
                    }
                self.push(Frame::video(ts, data), now);
            }
            Incoming::Audio { ts, data } if self.publishing => {
                if !self.seen_audio
                    && let Some(codec) = media::audio_codec(&data) {
                        self.seen_audio = true;
                        self.app.with_input(|i| i.audio = Some(codec));
                    }
                self.push(Frame::audio(ts, data), now);
            }
            Incoming::Data { ts, data } if self.publishing => {
                let (data, meta) = media::parse_data(data);
                let is_meta = meta.is_some();
                if let Some(meta) = meta {
                    self.app.with_input(|i| i.meta = meta);
                }
                self.push(Frame::data(ts, data, is_meta), now);
            }
            _ => {}
        }
        Ok(())
    }

    fn push(&self, frame: Frame, now: Instant) {
        self.app.bytes_in.fetch_add(frame.data.len() as u64, Relaxed);
        if self.app.engine.lock().unwrap().push(frame, now) {
            self.app.wake.notify_one();
        }
    }

    fn command(&mut self, conn: &mut Conn<TcpStream>, name: &str, tid: f64, object: &Amf0Value, args: &[Amf0Value], stream_id: u32) -> Result<()> {
        match name {
            "connect" => {
                self.app_name = rtmp::prop(object, "app").unwrap_or_default().to_string();
                conn.queue(RtmpMessage::WindowAcknowledgement { size: 5_000_000 }, 0, 0)?;
                conn.queue(RtmpMessage::SetPeerBandwidth { size: 5_000_000, limit_type: PeerBandwidthLimitType::Dynamic }, 0, 0)?;
                conn.set_chunk_size(4096)?;
                let props = obj(&[("fmsVer", s("FMS/3,0,1,123")), ("capabilities", n(31.0))]);
                let info = obj(&[
                    ("level", s("status")),
                    ("code", s("NetConnection.Connect.Success")),
                    ("description", s("Connection succeeded.")),
                    ("objectEncoding", n(0.0)),
                ]);
                conn.command("_result", tid, props, vec![info], 0)?;
            }
            "releaseStream" => conn.command("_result", tid, Amf0Value::Null, vec![Amf0Value::Undefined], 0)?,
            "FCPublish" => {
                let info = obj(&[("code", s("NetStream.Publish.Start")), ("description", s("FCPublish"))]);
                conn.command("onFCPublish", 0.0, Amf0Value::Null, vec![info], 0)?;
            }
            "createStream" => conn.command("_result", tid, Amf0Value::Null, vec![n(1.0)], 0)?,
            "publish" => {
                let key = match args.first() {
                    Some(Amf0Value::Utf8String(k)) => k.as_str(),
                    _ => "",
                };
                let sid = stream_id.max(1);
                if !self.app.begin_publish(&self.app_name, key, self.addr) {
                    let info = obj(&[
                        ("level", s("error")),
                        ("code", s("NetStream.Publish.BadName")),
                        ("description", s("DelayDeck is already receiving a stream")),
                    ]);
                    conn.command("onStatus", 0.0, Amf0Value::Null, vec![info], sid)?;
                    self.reject = Some("refused a second stream while one is already live");
                    return Ok(());
                }
                self.publishing = true;
                let begin = RtmpMessage::UserControl {
                    event_type: UserControlEventType::StreamBegin,
                    stream_id: Some(sid),
                    buffer_length: None,
                    timestamp: None,
                };
                conn.queue(begin, 0, 0)?;
                let info = obj(&[("level", s("status")), ("code", s("NetStream.Publish.Start")), ("description", s("Publishing."))]);
                conn.command("onStatus", 0.0, Amf0Value::Null, vec![info], sid)?;
            }
            "FCUnpublish" | "deleteStream" | "closeStream"
                if self.publishing => {
                    self.publishing = false;
                    self.app.end_publish();
                }
            _ => {}
        }
        Ok(())
    }
}
