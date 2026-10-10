use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::state::{App, Level, VERSION};

pub const REPO: Option<&str> = option_env!("DELAYDECK_UPDATE_REPO");
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Checking,
    Current,
    Available,
    Installing,
    Failed,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub enabled: bool,
    pub phase: Phase,
    pub latest: Option<String>,
    pub notes: Option<String>,
    pub error: Option<String>,
    pub checked: Option<u64>,
    #[serde(skip)]
    pub manifest: Option<Manifest>,
}

impl Status {
    pub fn new() -> Status {
        Status { enabled: REPO.is_some(), ..Status::default() }
    }
}

pub async fn run(app: Arc<App>) {
    tokio::spawn(remove_previous());
    if REPO.is_none() {
        return;
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        if app.config().auto_update {
            check(&app, false).await;
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

pub async fn check(app: &Arc<App>, manual: bool) {
    let Some(repo) = REPO else { return };
    app.with_update(|u| {
        u.phase = Phase::Checking;
        u.error = None;
    });
    let result = tokio::task::spawn_blocking(move || fetch_manifest(repo))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    let checked = Some(now_ms());
    match result {
        Ok(m) if is_newer(&m.version, VERSION) => {
            let fresh = app.update.lock().unwrap().latest.as_deref() != Some(m.version.as_str());
            let version = m.version.clone();
            app.with_update(|u| {
                u.phase = Phase::Available;
                u.latest = Some(m.version.clone());
                u.notes = m.notes.clone();
                u.manifest = Some(m);
                u.checked = checked;
            });
            if fresh || manual {
                app.log(Level::Ok, format!("DelayDeck {version} is available (you have {VERSION})"));
            }
        }
        Ok(_) => {
            app.with_update(|u| {
                u.phase = Phase::Current;
                u.latest = None;
                u.notes = None;
                u.manifest = None;
                u.checked = checked;
            });
            if manual {
                app.log(Level::Info, format!("DelayDeck {VERSION} is up to date"));
            }
        }
        Err(e) => {
            app.with_update(|u| {
                u.phase = Phase::Failed;
                u.error = Some(e.clone());
                u.checked = checked;
            });
            if manual {
                app.log(Level::Warn, format!("Update check failed: {e}"));
            }
        }
    }
}

pub async fn install(app: &Arc<App>) -> Result<(), String> {
    let repo = REPO.ok_or("updates are not enabled in this build")?;
    let manifest = app.update.lock().unwrap().manifest.clone().ok_or("no update is available")?;
    if !trusted(&manifest.url, repo) {
        return Err("the update points outside this project's GitHub releases".into());
    }
    app.with_update(|u| {
        u.phase = Phase::Installing;
        u.error = None;
    });
    app.log(Level::Info, format!("Downloading DelayDeck {}", manifest.version));
    let m = manifest.clone();
    let swapped = tokio::task::spawn_blocking(move || download_and_swap(&m))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    let exe = match swapped {
        Ok(exe) => exe,
        Err(e) => {
            app.with_update(|u| {
                u.phase = Phase::Available;
                u.error = Some(e.clone());
            });
            app.log(Level::Error, format!("Update failed: {e}"));
            return Err(e);
        }
    };
    if let Err(e) = relaunch(&exe) {
        let msg = format!("Update installed, but {e}. Restart DelayDeck to finish.");
        app.log(Level::Warn, msg.clone());
        return Err(msg);
    }
    app.log(Level::Ok, format!("Installed DelayDeck {}; restarting", manifest.version));
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        std::process::exit(0);
    });
    Ok(())
}

fn agent() -> ureq::Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(90)))
        .user_agent(format!("DelayDeck/{VERSION}"))
        .build()
        .into()
}

fn manifest_url(repo: &str) -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("DELAYDECK_UPDATE_MANIFEST") {
        return url;
    }
    format!("https://github.com/{repo}/releases/latest/download/latest.json")
}

fn trusted(url: &str, repo: &str) -> bool {
    if cfg!(debug_assertions) && std::env::var_os("DELAYDECK_UPDATE_MANIFEST").is_some() {
        return true;
    }
    url.starts_with(&format!("https://github.com/{repo}/releases/download/"))
}

fn fetch_manifest(repo: &str) -> Result<Manifest, String> {
    let mut res = agent().get(&manifest_url(repo)).call().map_err(describe)?;
    let text = res.body_mut().read_to_string().map_err(describe)?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("invalid update manifest: {e}"))
}

fn download_and_swap(m: &Manifest) -> Result<PathBuf, String> {
    let mut res = agent().get(&m.url).call().map_err(describe)?;
    let bytes = res.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec().map_err(describe)?;
    let digest: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if !digest.eq_ignore_ascii_case(m.sha256.trim()) {
        return Err("the download is corrupted (checksum mismatch)".into());
    }
    if !bytes.starts_with(b"MZ") {
        return Err("the download is not a Windows program".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let staged = sibling(&exe, ".new");
    let old = sibling(&exe, ".old");
    std::fs::write(&staged, &bytes).map_err(|e| no_write(&exe, e))?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(&exe, &old) {
        let _ = std::fs::remove_file(&staged);
        return Err(no_write(&exe, e));
    }
    if let Err(e) = std::fs::rename(&staged, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(no_write(&exe, e));
    }
    Ok(exe)
}

fn relaunch(exe: &Path) -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--relaunch").collect();
    args.push("--relaunch".into());
    Command::new(exe)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("it could not be started ({e})"))
}

async fn remove_previous() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = sibling(&exe, ".old");
    for _ in 0..40 {
        if !old.exists() || std::fs::remove_file(&old).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

fn no_write(exe: &Path, e: std::io::Error) -> String {
    let dir = exe.parent().map_or_else(|| ".".into(), |d| d.display().to_string());
    format!("can't replace the program in {dir} ({e}); move DelayDeck to a folder you can write to")
}

fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(404) => "no release has been published yet".into(),
        ureq::Error::StatusCode(code) => format!("GitHub answered HTTP {code}"),
        e => e.to_string(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn parse_version(v: &str) -> Option<[u64; 3]> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        *slot = match parts.next() {
            Some(p) => p.parse().ok()?,
            None => 0,
        };
    }
    parts.next().is_none().then_some(out)
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.9.12"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(is_newer("1.2", "1.1.9"));
    }

    #[test]
    fn only_trusts_this_repos_releases() {
        if std::env::var_os("DELAYDECK_UPDATE_MANIFEST").is_some() {
            return;
        }
        assert!(trusted("https://github.com/me/DelayDeck/releases/download/v1.0.0/DelayDeck.exe", "me/DelayDeck"));
        assert!(!trusted("https://github.com/evil/DelayDeck/releases/download/v1.0.0/DelayDeck.exe", "me/DelayDeck"));
        assert!(!trusted("http://github.com/me/DelayDeck/releases/download/v1.0.0/x.exe", "me/DelayDeck"));
        assert!(!trusted("https://example.com/me/DelayDeck/releases/download/x.exe", "me/DelayDeck"));
    }
}
