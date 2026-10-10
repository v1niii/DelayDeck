use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use rml_rtmp::messages::RtmpMessage;
use rml_rtmp::rml_amf0::Amf0Value;
use tokio::net::TcpStream;
use tokio::time::{sleep, sleep_until, timeout};

use crate::config::Service;
use crate::engine::{Link, OutFrame, Poll};
use crate::rtmp::{self, Conn, Incoming, Result, n, obj, s};
use crate::state::{App, Level, OutState};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const BACKOFF: [u64; 7] = [1, 2, 3, 5, 10, 15, 30];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dest {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub app: String,
    pub tc_url: String,
}

pub fn parse_url(url: &str) -> std::result::Result<Dest, String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or("the URL must start with rtmp:// or rtmps://")?;
    let scheme = scheme.to_ascii_lowercase();
    let tls = match scheme.as_str() {
        "rtmp" => false,
        "rtmps" => true,
        _ => return Err(format!("unsupported scheme \"{scheme}\" (use rtmp:// or rtmps://)")),
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let default_port = if tls { 443 } else { 1935 };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or("unterminated IPv6 address")?;
        let port = after.strip_prefix(':').map(str::parse).transpose().map_err(|_| "invalid port")?;
        (h.to_string(), port.unwrap_or(default_port))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "invalid port")?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err("the URL has no host".into());
    }
    let app = path.trim_matches('/').to_string();
    if app.is_empty() {
        return Err("the URL needs an application path, e.g. rtmp://host/app".into());
    }
    let tc_url = format!("{scheme}://{authority}/{app}");
    Ok(Dest { tls, host, port, app, tc_url })
}

pub struct Publisher {
    conn: Conn<Box<dyn rtmp::Stream>>,
    stream_id: u32,
    key: String,
    backlog: VecDeque<Incoming>,
}

impl Publisher {
    pub async fn connect(dest: &Dest, key: &str) -> Result<Publisher> {
        let tcp = TcpStream::connect((dest.host.as_str(), dest.port)).await?;
        tcp.set_nodelay(true)?;
        let io: Box<dyn rtmp::Stream> = if dest.tls {
            let tls = tokio_native_tls::TlsConnector::from(native_tls::TlsConnector::new()?);
            Box::new(tls.connect(&dest.host, tcp).await?)
        } else {
            Box::new(tcp)
        };
        let conn = Conn::open(io).await?;
        let mut p = Publisher { conn, stream_id: 0, key: key.to_string(), backlog: VecDeque::new() };

        p.conn.set_chunk_size(4096)?;
        let props = obj(&[
            ("app", s(&dest.app)),
            ("type", s("nonprivate")),
            ("flashVer", s("FMLE/3.0 (compatible; DelayDeck)")),
            ("swfUrl", s(&dest.tc_url)),
            ("tcUrl", s(&dest.tc_url)),
        ]);
        p.conn.command("connect", 1.0, props, vec![], 0)?;
        p.conn.flush().await?;
        p.expect_result(1.0, "connect").await?;

        p.conn.queue(RtmpMessage::WindowAcknowledgement { size: 2_500_000 }, 0, 0)?;
        p.conn.command("releaseStream", 2.0, Amf0Value::Null, vec![s(key)], 0)?;
        p.conn.command("FCPublish", 3.0, Amf0Value::Null, vec![s(key)], 0)?;
        p.conn.command("createStream", 4.0, Amf0Value::Null, vec![], 0)?;
        p.conn.flush().await?;
        let args = p.expect_result(4.0, "createStream").await?;
        p.stream_id = match args.first() {
            Some(Amf0Value::Number(id)) => *id as u32,
            _ => return Err("createStream returned no stream id".into()),
        };

        p.conn.command("publish", 5.0, Amf0Value::Null, vec![s(key), s("live")], p.stream_id)?;
        p.conn.flush().await?;
        loop {
            match p.next_command().await {
                Ok((name, _, args)) if name == "onStatus" => {
                    let info = args.first().cloned().unwrap_or(Amf0Value::Null);
                    let code = rtmp::prop(&info, "code").unwrap_or_default();
                    if code == "NetStream.Publish.Start" {
                        return Ok(p);
                    }
                    if rtmp::prop(&info, "level") == Some("error") {
                        let desc = rtmp::prop(&info, "description").unwrap_or_default();
                        return Err(format!("publish refused: {code} {desc}").trim().to_string().into());
                    }
                }
                Ok(_) => {}
                Err(e) if rtmp::is_closed(&e) => {
                    return Err("the server closed the connection when publishing; check your stream key".into());
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn next_command(&mut self) -> Result<(String, f64, Vec<Amf0Value>)> {
        loop {
            while let Some(msg) = self.backlog.pop_front() {
                if let Incoming::Command { name, tid, args, .. } = msg {
                    return Ok((name, tid, args));
                }
            }
            let msgs = self.conn.recv().await?;
            self.conn.flush().await?;
            self.backlog.extend(msgs);
        }
    }

    async fn expect_result(&mut self, tid: f64, what: &str) -> Result<Vec<Amf0Value>> {
        loop {
            let (name, id, args) = self.next_command().await?;
            if id != tid {
                continue;
            }
            match name.as_str() {
                "_result" => return Ok(args),
                "_error" => {
                    let info = args.first().cloned().unwrap_or(Amf0Value::Null);
                    let desc = rtmp::prop(&info, "description").or(rtmp::prop(&info, "code")).unwrap_or("no reason given");
                    return Err(format!("{what} rejected: {desc}").into());
                }
                _ => {}
            }
        }
    }

    pub fn queue(&mut self, f: OutFrame) -> Result<()> {
        self.conn.queue_media(f.kind.type_id(), f.ts, self.stream_id, f.data)
    }

    pub async fn flush(&mut self) -> Result<usize> {
        self.conn.flush().await
    }

    pub async fn recv(&mut self) -> Result<()> {
        for msg in self.conn.recv().await? {
            if let Incoming::Command { name, args, .. } = msg {
                let info = args.first().cloned().unwrap_or(Amf0Value::Null);
                if name == "onStatus" && rtmp::prop(&info, "level") == Some("error") {
                    let code = rtmp::prop(&info, "code").unwrap_or_default();
                    let desc = rtmp::prop(&info, "description").unwrap_or_default();
                    return Err(format!("server error: {code} {desc}").trim().to_string().into());
                }
            }
        }
        self.conn.flush().await?;
        Ok(())
    }

    pub async fn finish(mut self) {
        let key = std::mem::take(&mut self.key);
        let _ = self.conn.command("FCUnpublish", 6.0, Amf0Value::Null, vec![s(&key)], 0);
        let _ = self.conn.command("deleteStream", 7.0, Amf0Value::Null, vec![n(self.stream_id as f64)], 0);
        let _ = timeout(Duration::from_secs(2), self.conn.flush()).await;
        self.conn.shutdown().await;
    }
}

enum End {
    Finished,
    Restart,
}

pub async fn run(app: Arc<App>) {
    let mut failures = 0usize;
    loop {
        while !app.engine.lock().unwrap().wants_output(Instant::now()) {
            tokio::select! {
                _ = app.wake.notified() => {}
                _ = sleep(Duration::from_millis(250)) => {}
            }
        }

        let generation = app.dest_gen.load(Relaxed);
        let cfg = app.config();
        let label = cfg.service.label();
        if cfg.service == Service::Test {
            run_test(&app, generation).await;
            continue;
        }
        let key = app.stream_key.lock().unwrap().clone().unwrap_or_default();
        let dest = match cfg.destination().as_deref().map(parse_url) {
            Some(Ok(dest)) => dest,
            Some(Err(e)) => {
                offline(&app, format!("Invalid destination URL: {e}"), None, Duration::from_secs(3));
                wait_retry(&app, generation, Duration::from_secs(3)).await;
                continue;
            }
            None => {
                offline(&app, "No destination URL: choose a service in Settings".into(), None, Duration::from_secs(3));
                wait_retry(&app, generation, Duration::from_secs(3)).await;
                continue;
            }
        };

        app.engine.lock().unwrap().set_link(Link::Connecting);
        app.set_output(OutState::Connecting, Some(dest.host.clone()), None, None);
        app.log(Level::Info, format!("Connecting to {label} ({})", dest.host));
        let connected = match timeout(CONNECT_TIMEOUT, Publisher::connect(&dest, &key)).await {
            Ok(result) => result,
            Err(_) => Err("connection timed out".into()),
        };
        let mut publisher = match connected {
            Ok(p) => p,
            Err(e) => {
                let wait = Duration::from_secs(BACKOFF[failures.min(BACKOFF.len() - 1)]);
                failures += 1;
                offline(&app, format!("Could not connect to {label}: {e}"), Some(dest.host.clone()), wait);
                wait_retry(&app, generation, wait).await;
                continue;
            }
        };

        failures = 0;
        app.engine.lock().unwrap().output_connected(Instant::now());
        app.set_output(OutState::Live, Some(dest.host.clone()), None, None);
        app.log(Level::Ok, format!("Live on {label}"));

        match stream(&app, Some(&mut publisher), generation).await {
            Ok(End::Finished) => {
                publisher.finish().await;
                app.engine.lock().unwrap().output_finished();
                app.set_output(OutState::Idle, None, None, None);
                app.log(Level::Info, format!("Delayed stream finished; disconnected from {label}"));
            }
            Ok(End::Restart) => {
                publisher.finish().await;
                app.engine.lock().unwrap().set_link(Link::Idle);
                app.set_output(OutState::Idle, None, None, None);
                app.log(Level::Info, "Destination changed; reconnecting");
            }
            Err(e) => {
                let wait = Duration::from_secs(BACKOFF[0]);
                failures += 1;
                offline(&app, format!("Lost connection to {label}: {e}"), Some(dest.host), wait);
                wait_retry(&app, generation, wait).await;
            }
        }
    }
}

async fn run_test(app: &App, generation: u64) {
    app.engine.lock().unwrap().output_connected(Instant::now());
    app.set_output(OutState::Live, Some("this PC (test)".into()), None, None);
    app.log(Level::Ok, "Test mode: delaying locally; nothing is sent to any platform");
    match stream(app, None, generation).await {
        Ok(End::Restart) => {
            app.engine.lock().unwrap().set_link(Link::Idle);
            app.set_output(OutState::Idle, None, None, None);
        }
        _ => {
            app.engine.lock().unwrap().output_finished();
            app.set_output(OutState::Idle, None, None, None);
            app.log(Level::Info, "Test stream finished");
        }
    }
}

fn offline(app: &App, msg: String, host: Option<String>, retry_in: Duration) {
    app.engine.lock().unwrap().set_link(Link::Offline);
    app.log(Level::Error, format!("{msg} (retrying in {}s)", retry_in.as_secs()));
    app.set_output(OutState::Retrying, host, Some(msg), Some(retry_in));
}

async fn wait_retry(app: &App, generation: u64, wait: Duration) {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        tokio::select! {
            _ = sleep_until(deadline) => return,
            _ = app.wake.notified() => {
                if app.dest_gen.load(Relaxed) != generation {
                    return;
                }
            }
        }
    }
}

async fn stream(app: &App, mut p: Option<&mut Publisher>, generation: u64) -> Result<End> {
    loop {
        if app.dest_gen.load(Relaxed) != generation {
            return Ok(End::Restart);
        }
        let poll = app.engine.lock().unwrap().poll(Instant::now());
        let deadline = match poll {
            Poll::Ready(frames) => {
                let mut size = 0;
                for f in frames {
                    app.preview.tap(&f);
                    size += f.data.len();
                    if let Some(p) = p.as_deref_mut() {
                        p.queue(f)?;
                    }
                }
                if let Some(p) = p.as_deref_mut() {
                    size = timeout(WRITE_TIMEOUT, p.flush()).await.map_err(|_| "the destination stopped accepting data")??;
                }
                app.bytes_out.fetch_add(size as u64, Relaxed);
                continue;
            }
            Poll::Wait(at) => tokio::time::Instant::from_std(at),
            Poll::Idle => tokio::time::Instant::now() + Duration::from_secs(1),
            Poll::Done => return Ok(End::Finished),
        };
        tokio::select! {
            r = recv(p.as_deref_mut()) => r?,
            _ = app.wake.notified() => {}
            _ = sleep_until(deadline) => {}
        }
    }
}

async fn recv(p: Option<&mut Publisher>) -> Result<()> {
    match p {
        Some(p) => p.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ingest_urls() {
        let d = parse_url("rtmp://live.twitch.tv/app").unwrap();
        assert_eq!((d.tls, d.host.as_str(), d.port, d.app.as_str()), (false, "live.twitch.tv", 1935, "app"));
        let d = parse_url("rtmps://fa723fc1b171.global-contribute.live-video.net:443/app/").unwrap();
        assert_eq!((d.tls, d.port, d.app.as_str()), (true, 443, "app"));
        assert_eq!(d.tc_url, "rtmps://fa723fc1b171.global-contribute.live-video.net:443/app");
        let d = parse_url("rtmp://[::1]:1940/live/sub").unwrap();
        assert_eq!((d.host.as_str(), d.port, d.app.as_str()), ("::1", 1940, "live/sub"));
        assert!(parse_url("http://example.com/app").is_err());
        assert!(parse_url("rtmp://example.com").is_err());
    }
}
