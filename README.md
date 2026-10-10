# DelayDeck

A small RTMP relay that sits between OBS and your streaming platform and holds the stream back by a delay you can change at any time, with a web control panel (works as an OBS dock) and a customizable on-stream overlay.

- One ~2 MB executable, no runtime dependencies, a few MB of RAM plus the buffer itself
- Updates itself from GitHub Releases
- Any delay from 0s up to a configurable maximum (10 min by default), changed live
- Twitch, YouTube, Kick or any custom `rtmp://` / `rtmps://` server, or a local Test mode that sends nothing
- Built-in output preview: watch exactly what viewers get
- H.264, HEVC and AV1 (Enhanced RTMP), AAC and Opus pass through untouched (no re-encoding)

## Setup (one time)

1. Run `delaydeck.exe`. The control panel opens at <http://127.0.0.1:3000/>.
2. In OBS: **Settings → Stream → Service: `Custom...`**
   - Server: `rtmp://127.0.0.1:1935/live`
   - Stream key: keep your platform's stream key. DelayDeck passes it on.
3. In the panel's **Settings** tab, choose your platform (Twitch is the default).

Optional:

- **Overlay**: add a Browser Source in OBS with `http://127.0.0.1:3000/overlay`, 1920×1080.
- **Dock**: in OBS, **Docks → Custom Browser Docks**, add `http://127.0.0.1:3000/`.

Set OBS's keyframe interval to 2s (Settings → Output → Advanced). Skips and replays land on keyframes, so this keeps them precise.

## Testing without going live

1. In **Settings**, choose **Test** as the destination. The whole delay pipeline runs, but nothing is sent to any platform.
2. Start streaming in OBS (the stream key can be anything).
3. On the **Control** tab, press **Watch output** to see the delayed stream next to OBS's own preview. Change the delay, skip to live, and see exactly what viewers would get.

The same preview is available at `http://127.0.0.1:3000/preview.flv` for VLC, ffplay or an OBS Media Source (in a separate scene, so it doesn't end up inside your stream). It works with every destination, not only Test.

## How the delay works

- **Start**: when you go live, the platform receives your stream after the delay you set.
- **Lowering the delay** (including **Skip to live**) jumps ahead to the nearest keyframe. Viewers miss those seconds.
- **Raising the delay** needs a buffer that long first. You choose how (Settings → *Raising the delay*):
  - **Replay** (default): viewers keep watching while the buffer fills, then jump back and see those seconds again. No freeze.
  - **Pause**: the output holds until the buffer is full. Viewers see a freeze, but nothing repeats.
- **Stopping** in OBS: the buffered seconds still play out, then DelayDeck disconnects from the platform.
- If the platform connection drops, DelayDeck reconnects with backoff and resumes at the right delay.

Outgoing timestamps are rewritten so the platform always sees one continuous stream across skips, replays and OBS reconnects.

## Overlay

Style (Pill, Minimal, Bar, Badge), position, accent color, labels, size and margin are set in the **Overlay** tab and apply to OBS instantly. URL parameters override them per browser source:

| Parameter | Example |
| --- | --- |
| `style` | `pill`, `minimal`, `bar`, `badge` |
| `pos` | `top-left`, `top-center`, `top-right`, `bottom-left`, `bottom-center`, `bottom-right` |
| `accent` | `ff4757` |
| `scale` | `1.5` |
| `margin` | `24` |
| `label` / `live` | `DELAY` / `LIVE` |
| `showlive` / `progress` | `1` or `0` |

Example: `http://127.0.0.1:3000/overlay?style=bar&pos=bottom-left&accent=22d3ee`

## Control API

For Stream Deck, Bitfocus Companion or scripts. Writes must be JSON (`Content-Type: application/json`), which keeps web pages from triggering them.

| Request | Effect |
| --- | --- |
| `POST /api/delay` `{"seconds": 30}` | Set the delay |
| `POST /api/delay` `{"delta": 5}` | Add or remove seconds |
| `POST /api/skip` `{}` | Skip to live (delay 0) |
| `GET /api/status` | Current state as JSON |
| `GET /api/events` | Server-sent events: `status`, `config`, `log` |

```powershell
Invoke-RestMethod -Method Post http://127.0.0.1:3000/api/delay -ContentType application/json -Body '{"seconds":30}'
```

Keyboard shortcuts in the panel: `1`–`9` presets, `L` skip to live, `?` manual.

## Configuration

Settings are saved to `%APPDATA%\DelayDeck\config.json` (see the Settings tab for the exact path). Ports and the listen address are set there or on the command line:

```
delaydeck [--config PATH] [--bind ADDR] [--rtmp-port 1935] [--http-port 3000] [--no-browser]
```

DelayDeck listens on `127.0.0.1` only by default. Set `"bind": "0.0.0.0"` to accept OBS from another machine. The control panel has no password, so do that only on a trusted network.

The buffer is held in memory: roughly bitrate × delay (60s at 6 Mbps ≈ 45 MB).

## Updates

Release builds check GitHub Releases for a newer version when they start and every 6 hours (Settings → App → *Check for updates automatically*). When one exists, a bar at the top of the panel offers **Update now**. It's disabled while you're streaming, since updating restarts DelayDeck. The update downloads the new exe from this repository's releases, verifies its SHA-256, swaps it in place, restarts on the same ports, and the panel reloads by itself.

## Releasing

Push a version tag:

```
git tag v0.2.0
git push origin v0.2.0
```

The `Release` workflow takes the version from the tag, runs the tests, builds, and publishes a GitHub release with:

- `DelayDeck-<version>-windows-x64.zip`: what people download
- `DelayDeck-<version>-windows-x64.exe` and `latest.json`: what the updater uses

The repository must be public so the updater can download releases without a login.

## Building

```
cargo build --release        # target\release\delaydeck.exe
cargo test                   # unit tests + an end-to-end relay test over real sockets
cargo run --example fake_obs # publishes a synthetic stream, to try the panel without OBS
.\package.ps1                # dist\DelayDeck-<version>-windows-x64.zip, ready to share
```

The zip holds `DelayDeck.exe`, a quick-start `README.txt` for users and `THIRD-PARTY-NOTICES.txt`. The exe only uses DLLs that ship with Windows, so users just unzip and run it. Local builds don't update themselves; the release workflow enables that by building with `DELAYDECK_UPDATE_REPO` set to the repository.

On Windows with the GNU toolchain, building needs MinGW-w64 binutils on `PATH` (for example `winget install BrechtSanders.WinLibs.POSIX.MSVCRT`). With the MSVC toolchain, Visual Studio Build Tools are enough.

## Layout

| Path | What it does |
| --- | --- |
| `src/engine.rs` | Delay buffer, playback cursor, skip/replay/pause logic, timestamp rewriting |
| `src/ingest.rs` | RTMP server OBS publishes to |
| `src/output.rs` | RTMP(S) publisher to the platform, with reconnect |
| `src/rtmp.rs` | Handshake, chunking and command plumbing (on `rml_rtmp`) |
| `src/media.rs` | FLV tag inspection: keyframes, codec headers, metadata |
| `src/preview.rs` | Mirrors the delayed output as live HTTP-FLV for the preview player |
| `src/update.rs` | Checks GitHub Releases, downloads, verifies and swaps in new versions |
| `.github/workflows/release.yml` | Builds and publishes a release when a `v*` tag is pushed |
| `src/web.rs`, `web/` | Control panel, overlay and JSON/SSE API |
| `web/vendor/mpegts.js` | [mpegts.js](https://github.com/xqq/mpegts.js) 1.7.3 (Apache-2.0), the preview player |
