use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::json;
use tokio::sync::{Notify, watch};

use crate::config::{self, Config, Mode, Patch, Service};
use crate::engine::{Engine, Snapshot};
use crate::media::MetaInfo;
use crate::preview::Preview;
use crate::update;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

#[derive(Serialize)]
struct LogLine {
    t: u64,
    level: Level,
    msg: String,
}

pub struct InputInfo {
    pub app: String,
    pub addr: SocketAddr,
    pub since: Instant,
    pub video: Option<&'static str>,
    pub audio: Option<&'static str>,
    pub meta: MetaInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutState {
    Idle,
    Connecting,
    Live,
    Retrying,
}

pub struct OutputInfo {
    pub state: OutState,
    pub since: Instant,
    pub service: Service,
    pub host: Option<String>,
    pub error: Option<String>,
    pub retry_at: Option<Instant>,
}

pub struct App {
    pub engine: Mutex<Engine>,
    pub wake: Notify,
    pub dest_gen: AtomicU64,
    pub config: Mutex<Config>,
    config_path: PathBuf,
    pub input: Mutex<Option<InputInfo>>,
    pub stream_key: Mutex<Option<String>>,
    pub output: Mutex<OutputInfo>,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub kbps_in: AtomicU32,
    pub kbps_out: AtomicU32,
    pub preview: Preview,
    pub update: Mutex<update::Status>,
    log: Mutex<VecDeque<LogLine>>,
    pub status_tx: watch::Sender<Arc<str>>,
    pub config_tx: watch::Sender<Arc<str>>,
    pub log_tx: watch::Sender<Arc<str>>,
    started: Instant,
}

impl App {
    pub fn new(config: Config, config_path: PathBuf) -> Arc<App> {
        let engine = Engine::new(Duration::from_secs_f64(config.delay), config.mode);
        let service = config.service;
        let app = Arc::new(App {
            engine: Mutex::new(engine),
            wake: Notify::new(),
            dest_gen: AtomicU64::new(0),
            config: Mutex::new(config),
            config_path,
            input: Mutex::new(None),
            stream_key: Mutex::new(None),
            output: Mutex::new(OutputInfo {
                state: OutState::Idle,
                since: Instant::now(),
                service,
                host: None,
                error: None,
                retry_at: None,
            }),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            kbps_in: AtomicU32::new(0),
            kbps_out: AtomicU32::new(0),
            preview: Preview::default(),
            update: Mutex::new(update::Status::new()),
            log: Mutex::new(VecDeque::new()),
            status_tx: watch::channel(Arc::from("{}")).0,
            config_tx: watch::channel(Arc::from("{}")).0,
            log_tx: watch::channel(Arc::from("[]")).0,
            started: Instant::now(),
        });
        app.publish_config();
        app.publish_status();
        app
    }

    pub fn log(&self, level: Level, msg: impl Into<String>) {
        let msg = msg.into();
        let up = self.started.elapsed().as_secs();
        let tag = match level {
            Level::Info => "    ",
            Level::Ok => " ok ",
            Level::Warn => "warn",
            Level::Error => "fail",
        };
        eprintln!("[{:02}:{:02}:{:02}] {tag} {msg}", up / 3600, up / 60 % 60, up % 60);
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let json = {
            let mut log = self.log.lock().unwrap();
            log.push_back(LogLine { t, level, msg });
            while log.len() > 80 {
                log.pop_front();
            }
            serde_json::to_string(&*log).expect("log serializes")
        };
        self.log_tx.send_replace(json.into());
    }

    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let snapshot = {
            let mut c = self.config.lock().unwrap();
            f(&mut c);
            c.sanitize();
            c.clone()
        };
        if let Err(e) = config::save(&self.config_path, &snapshot) {
            self.log(Level::Warn, format!("Could not save settings to {}: {e}", self.config_path.display()));
        }
        self.publish_config();
    }

    pub fn apply_patch(&self, patch: Patch) {
        let before = self.config();
        self.update_config(|c| patch.apply(c));
        let after = self.config();
        if (after.service, after.destination()) != (before.service, before.destination()) {
            self.dest_gen.fetch_add(1, Relaxed);
            self.log(Level::Info, format!("Destination set to {}", describe_destination(&after)));
        }
        if after.mode != before.mode {
            self.engine.lock().unwrap().set_mode(after.mode);
            let how = match after.mode {
                Mode::Replay => "replay (stream keeps flowing, then jumps back)",
                Mode::Pause => "pause (output holds until the buffer is full)",
            };
            self.log(Level::Info, format!("Delay increases now use {how}"));
        }
        if after.delay != before.delay {
            self.engine.lock().unwrap().set_target(Duration::from_secs_f64(after.delay), Instant::now());
        }
        self.wake.notify_one();
        self.publish_status();
    }

    pub fn set_delay(&self, secs: f64) -> f64 {
        let max = self.config.lock().unwrap().max_delay;
        let secs = (secs.clamp(0.0, max) * 10.0).round() / 10.0;
        let changed = {
            let mut engine = self.engine.lock().unwrap();
            let changed = engine.target() != Duration::from_secs_f64(secs);
            engine.set_target(Duration::from_secs_f64(secs), Instant::now());
            changed
        };
        if changed {
            self.update_config(|c| c.delay = secs);
            if secs == 0.0 {
                self.log(Level::Info, "Delay removed: skipping to live");
            } else {
                self.log(Level::Info, format!("Delay set to {}", fmt_secs(secs)));
            }
        }
        self.wake.notify_one();
        self.publish_status();
        secs
    }

    pub fn begin_publish(&self, app_name: &str, key: &str, addr: SocketAddr) -> bool {
        {
            let mut input = self.input.lock().unwrap();
            if input.is_some() {
                return false;
            }
            *input = Some(InputInfo {
                app: app_name.to_string(),
                addr,
                since: Instant::now(),
                video: None,
                audio: None,
                meta: MetaInfo::default(),
            });
        }
        *self.stream_key.lock().unwrap() = Some(key.to_string());
        self.engine.lock().unwrap().begin_input(Instant::now());
        self.log(Level::Ok, format!("OBS started streaming (from {}, app \"{app_name}\")", addr.ip()));
        if key.trim().is_empty() {
            self.log(Level::Warn, "OBS sent an empty stream key: put your platform's stream key in OBS");
        }
        self.wake.notify_one();
        true
    }

    pub fn end_publish(&self) {
        if self.input.lock().unwrap().take().is_none() {
            return;
        }
        self.engine.lock().unwrap().end_input();
        self.log(Level::Info, "OBS stopped streaming; sending what's left in the buffer");
        self.wake.notify_one();
    }

    pub fn with_input(&self, f: impl FnOnce(&mut InputInfo)) {
        if let Some(input) = self.input.lock().unwrap().as_mut() {
            f(input);
        }
    }

    pub fn with_update(&self, f: impl FnOnce(&mut update::Status)) {
        f(&mut self.update.lock().unwrap());
        self.publish_status();
    }

    pub fn is_streaming(&self) -> bool {
        self.input.lock().unwrap().is_some() || self.output.lock().unwrap().state != OutState::Idle
    }

    pub fn set_output(&self, state: OutState, host: Option<String>, error: Option<String>, retry_in: Option<Duration>) {
        let service = self.config.lock().unwrap().service;
        let mut o = self.output.lock().unwrap();
        if o.state != state {
            o.since = Instant::now();
        }
        o.state = state;
        o.service = service;
        o.host = host;
        o.error = error;
        o.retry_at = retry_in.map(|d| Instant::now() + d);
    }

    pub fn publish_status(&self) {
        let json = self.status_json();
        self.status_tx.send_if_modified(|cur| {
            if **cur != *json {
                *cur = json.into();
                true
            } else {
                false
            }
        });
    }

    fn status_json(&self) -> String {
        #[derive(Serialize)]
        struct Status {
            delay: Snapshot,
            mode: Mode,
            max_delay: f64,
            input: Option<InputView>,
            output: OutputView,
            update: update::Status,
        }
        #[derive(Serialize)]
        struct InputView {
            app: String,
            addr: String,
            uptime: u64,
            kbps: u32,
            video: Option<&'static str>,
            audio: Option<&'static str>,
            width: Option<u32>,
            height: Option<u32>,
            fps: Option<f64>,
            encoder: Option<String>,
        }
        #[derive(Serialize)]
        struct OutputView {
            state: OutState,
            service: &'static str,
            local: bool,
            host: Option<String>,
            error: Option<String>,
            uptime: u64,
            retry_in: Option<u64>,
            kbps: u32,
        }

        let now = Instant::now();
        let delay = self.engine.lock().unwrap().snapshot(now);
        let (mode, max_delay) = {
            let c = self.config.lock().unwrap();
            (c.mode, c.max_delay)
        };
        let input = self.input.lock().unwrap().as_ref().map(|i| InputView {
            app: i.app.clone(),
            addr: i.addr.ip().to_string(),
            uptime: now.duration_since(i.since).as_secs(),
            kbps: self.kbps_in.load(Relaxed),
            video: i.video,
            audio: i.audio,
            width: i.meta.width,
            height: i.meta.height,
            fps: i.meta.fps.map(|f| (f * 100.0).round() / 100.0),
            encoder: i.meta.encoder.clone(),
        });
        let output = {
            let o = self.output.lock().unwrap();
            OutputView {
                state: o.state,
                service: o.service.label(),
                local: o.service == Service::Test,
                host: o.host.clone(),
                error: o.error.clone(),
                uptime: now.duration_since(o.since).as_secs(),
                retry_in: o.retry_at.map(|t| t.saturating_duration_since(now).as_secs_f64().ceil() as u64),
                kbps: self.kbps_out.load(Relaxed),
            }
        };
        let update = self.update.lock().unwrap().clone();
        serde_json::to_string(&Status { delay, mode, max_delay, input, output, update }).expect("status serializes")
    }

    pub fn publish_config(&self) {
        let c = self.config();
        let host = match c.bind.as_str() {
            "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
            other => other.to_string(),
        };
        let services: Vec<_> = Service::ALL
            .iter()
            .map(|s| json!({ "id": s, "label": s.label(), "url": s.ingest() }))
            .collect();
        let json = json!({
            "version": VERSION,
            "config": c,
            "destination": c.destination(),
            "services": services,
            "urls": {
                "server": format!("rtmp://{host}:{}/live", c.rtmp_port),
                "panel": format!("http://{host}:{}/", c.http_port),
                "overlay": format!("http://{host}:{}/overlay", c.http_port),
                "preview": format!("http://{host}:{}/preview.flv", c.http_port),
            },
            "config_path": self.config_path.display().to_string(),
        });
        self.config_tx.send_replace(json.to_string().into());
    }
}

pub async fn status_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut last = (Instant::now(), 0u64, 0u64);
    loop {
        tick.tick().await;
        let now = Instant::now();
        let dt = now.duration_since(last.0).as_secs_f64();
        if dt >= 1.0 {
            let (bytes_in, bytes_out) = (app.bytes_in.load(Relaxed), app.bytes_out.load(Relaxed));
            let kbps = |bytes: u64, prev: u64| ((bytes - prev) as f64 * 8.0 / 1000.0 / dt).round() as u32;
            app.kbps_in.store(kbps(bytes_in, last.1), Relaxed);
            app.kbps_out.store(kbps(bytes_out, last.2), Relaxed);
            last = (now, bytes_in, bytes_out);
        }
        app.publish_status();
        app.wake.notify_one();
    }
}

pub fn fmt_secs(secs: f64) -> String {
    if secs < 60.0 {
        let s = format!("{secs:.1}");
        format!("{}s", s.trim_end_matches(".0"))
    } else {
        let m = (secs / 60.0).floor();
        let r = (secs - m * 60.0).round();
        if r == 0.0 { format!("{m}m") } else { format!("{m}m {r}s") }
    }
}

pub fn describe_destination(c: &Config) -> String {
    match (c.service, c.destination()) {
        (Service::Test, _) => "Test (nothing leaves this PC; watch the output preview)".into(),
        (s, Some(url)) => format!("{} ({url})", s.label()),
        (s, None) => format!("{} (no server URL set)", s.label()),
    }
}
