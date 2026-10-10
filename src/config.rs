use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Replay,
    Pause,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    #[default]
    Twitch,
    Youtube,
    Kick,
    Custom,
    Test,
}

impl Service {
    pub const ALL: [Service; 5] = [Service::Twitch, Service::Youtube, Service::Kick, Service::Custom, Service::Test];

    pub fn label(self) -> &'static str {
        match self {
            Service::Twitch => "Twitch",
            Service::Youtube => "YouTube",
            Service::Kick => "Kick",
            Service::Custom => "Custom",
            Service::Test => "Test",
        }
    }

    pub fn ingest(self) -> Option<&'static str> {
        match self {
            Service::Twitch => Some("rtmp://live.twitch.tv/app"),
            Service::Youtube => Some("rtmp://a.rtmp.youtube.com/live2"),
            Service::Kick => Some("rtmps://fa723fc1b171.global-contribute.live-video.net/app"),
            Service::Custom | Service::Test => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Style {
    #[default]
    Pill,
    Minimal,
    Bar,
    Badge,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Position {
    TopLeft,
    TopCenter,
    #[default]
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    pub style: Style,
    pub position: Position,
    pub accent: String,
    pub label: String,
    pub live_label: String,
    pub show_when_live: bool,
    pub show_progress: bool,
    pub scale: f64,
    pub margin: u32,
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay {
            style: Style::Pill,
            position: Position::TopRight,
            accent: "#8b5cf6".into(),
            label: "DELAY".into(),
            live_label: "LIVE".into(),
            show_when_live: false,
            show_progress: true,
            scale: 1.0,
            margin: 48,
        }
    }
}

impl Overlay {
    fn sanitize(&mut self) {
        let hex = self.accent.strip_prefix('#').unwrap_or("");
        if !matches!(hex.len(), 3 | 6) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            self.accent = Overlay::default().accent;
        }
        self.label = clip(&self.label, 24);
        self.live_label = clip(&self.live_label, 24);
        self.scale = if self.scale.is_finite() { self.scale.clamp(0.5, 3.0) } else { 1.0 };
        self.margin = self.margin.min(400);
    }
}

fn clip(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bind: String,
    pub rtmp_port: u16,
    pub http_port: u16,
    pub open_browser: bool,
    pub auto_update: bool,
    pub service: Service,
    pub custom_url: String,
    pub delay: f64,
    pub mode: Mode,
    pub presets: Vec<f64>,
    pub max_delay: f64,
    pub overlay: Overlay,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: "127.0.0.1".into(),
            rtmp_port: 1935,
            http_port: 3000,
            open_browser: true,
            auto_update: true,
            service: Service::Twitch,
            custom_url: String::new(),
            delay: 0.0,
            mode: Mode::Replay,
            presets: vec![0.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0],
            max_delay: 600.0,
            overlay: Overlay::default(),
        }
    }
}

impl Config {
    pub fn destination(&self) -> Option<String> {
        match self.service {
            Service::Test => None,
            Service::Custom => {
                let url = self.custom_url.trim();
                (!url.is_empty()).then(|| url.to_string())
            }
            s => s.ingest().map(str::to_string),
        }
    }

    pub fn sanitize(&mut self) {
        self.max_delay = if self.max_delay.is_finite() { self.max_delay.clamp(10.0, 3600.0) } else { 600.0 };
        self.delay = if self.delay.is_finite() { self.delay.clamp(0.0, self.max_delay) } else { 0.0 };
        let max = self.max_delay;
        self.presets.retain(|p| p.is_finite() && (0.0..=max).contains(p));
        for p in &mut self.presets {
            *p = (*p * 10.0).round() / 10.0;
        }
        self.presets.sort_by(f64::total_cmp);
        self.presets.dedup();
        self.presets.truncate(12);
        self.custom_url = self.custom_url.trim().to_string();
        self.overlay.sanitize();
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Patch {
    pub service: Option<Service>,
    pub custom_url: Option<String>,
    pub mode: Option<Mode>,
    pub presets: Option<Vec<f64>>,
    pub max_delay: Option<f64>,
    pub open_browser: Option<bool>,
    pub auto_update: Option<bool>,
    pub overlay: Option<Overlay>,
}

impl Patch {
    pub fn apply(self, c: &mut Config) {
        if let Some(v) = self.service {
            c.service = v;
        }
        if let Some(v) = self.custom_url {
            c.custom_url = v;
        }
        if let Some(v) = self.mode {
            c.mode = v;
        }
        if let Some(v) = self.presets {
            c.presets = v;
        }
        if let Some(v) = self.max_delay {
            c.max_delay = v;
        }
        if let Some(v) = self.open_browser {
            c.open_browser = v;
        }
        if let Some(v) = self.auto_update {
            c.auto_update = v;
        }
        if let Some(v) = self.overlay {
            c.overlay = v;
        }
        c.sanitize();
    }
}

pub fn default_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("APPDATA") {
        return PathBuf::from(dir).join("DelayDeck").join("config.json");
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("delaydeck").join("config.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("delaydeck").join("config.json");
    }
    PathBuf::from("delaydeck.json")
}

pub fn load(path: &Path) -> (Config, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Config>(&text) {
            Ok(mut cfg) => {
                cfg.sanitize();
                (cfg, None)
            }
            Err(e) => {
                let backup = path.with_extension("json.bak");
                let _ = std::fs::rename(path, &backup);
                let warning = format!("Config was invalid ({e}); saved a copy to {} and reset to defaults", backup.display());
                (Config::default(), Some(warning))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
        Err(e) => (Config::default(), Some(format!("Could not read {}: {e}", path.display()))),
    }
}

pub fn save(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg).expect("config serializes"))?;
    std::fs::rename(&tmp, path)
}
