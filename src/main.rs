use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use delaydeck::config;
use delaydeck::state::{self, App, Level, VERSION};
use delaydeck::{ingest, output, update, web};
use tokio::net::TcpListener;

const HELP: &str = "\
DelayDeck: adjustable stream delay between OBS and your platform

USAGE:
    delaydeck [OPTIONS]

OPTIONS:
    --config <PATH>     Settings file (default: %APPDATA%\\DelayDeck\\config.json)
    --bind <ADDR>       Address to listen on (default: 127.0.0.1)
    --rtmp-port <PORT>  RTMP port OBS streams to (default: 1935)
    --http-port <PORT>  Control panel port (default: 3000)
    --no-browser        Don't open the control panel on start
    -h, --help          Show this help
    -V, --version       Show the version
";

struct Args {
    config: Option<PathBuf>,
    bind: Option<String>,
    rtmp_port: Option<u16>,
    http_port: Option<u16>,
    no_browser: bool,
    relaunch: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { config: None, bind: None, rtmp_port: None, http_port: None, no_browser: false, relaunch: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--config" => args.config = Some(value("--config")?.into()),
            "--bind" => args.bind = Some(value("--bind")?),
            "--rtmp-port" => args.rtmp_port = Some(value("--rtmp-port")?.parse().map_err(|_| "invalid --rtmp-port")?),
            "--http-port" => args.http_port = Some(value("--http-port")?.parse().map_err(|_| "invalid --http-port")?),
            "--no-browser" => args.no_browser = true,
            "--relaunch" => args.relaunch = true,
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("delaydeck {VERSION}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other} (see --help)")),
        }
    }
    Ok(args)
}

async fn listen(bind: &str, port: u16, patient: bool) -> std::io::Result<TcpListener> {
    let deadline = Instant::now() + Duration::from_secs(if patient { 15 } else { 0 });
    loop {
        match TcpListener::bind((bind, port)).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            result => return result,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let path = args.config.unwrap_or_else(config::default_path);
    let (mut cfg, warning) = config::load(&path);
    if let Some(bind) = args.bind {
        cfg.bind = bind;
    }
    if let Some(port) = args.rtmp_port {
        cfg.rtmp_port = port;
    }
    if let Some(port) = args.http_port {
        cfg.http_port = port;
    }
    let open_browser = cfg.open_browser && !args.no_browser && !args.relaunch;
    let (bind, rtmp_port, http_port) = (cfg.bind.clone(), cfg.rtmp_port, cfg.http_port);
    let app = App::new(cfg, path);
    let urls: serde_json::Value = serde_json::from_str(&app.config_tx.borrow()).expect("config json");
    let url = |k: &str| urls["urls"][k].as_str().unwrap_or_default().to_string();

    let http = match listen(&bind, http_port, args.relaunch).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("Port {http_port} is already in use. DelayDeck may already be running at {}", url("panel"));
            if open_browser {
                open_url(&url("panel"));
            }
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("error: could not listen on {bind}:{http_port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rtmp = match listen(&bind, rtmp_port, args.relaunch).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: could not listen for RTMP on {bind}:{rtmp_port}: {e}");
            eprintln!("Is another RTMP server running? Pick another port with --rtmp-port.");
            return ExitCode::FAILURE;
        }
    };

    eprintln!();
    eprintln!("  DelayDeck v{VERSION}");
    eprintln!("  ├─ Control panel   {}", url("panel"));
    eprintln!("  ├─ OBS overlay     {}", url("overlay"));
    eprintln!("  ├─ Output preview  {}", url("preview"));
    eprintln!("  └─ OBS server      {}   (stream key: your platform key)", url("server"));
    eprintln!();
    if args.relaunch {
        app.log(Level::Ok, format!("Updated to DelayDeck {VERSION}"));
    }
    if let Some(w) = warning {
        app.log(Level::Warn, w);
    }
    let cfg = app.config();
    let dest = state::describe_destination(&cfg);
    app.log(Level::Info, format!("Destination: {dest} · delay {}", state::fmt_secs(cfg.delay)));

    tokio::spawn(ingest::run(app.clone(), rtmp));
    tokio::spawn(output::run(app.clone()));
    tokio::spawn(state::status_loop(app.clone()));
    tokio::spawn(update::run(app.clone()));
    if open_browser {
        open_url(&url("panel"));
    }

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Err(e) = axum::serve(http, web::router(app.clone())).with_graceful_shutdown(shutdown).await {
        eprintln!("error: web server stopped: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn open_url(url: &str) {
    #[cfg(windows)]
    let result = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = result {
        eprintln!("Could not open a browser ({e}); visit {url}");
    }
}
