# DelayDeck

Adjustable stream delay for OBS. It sits between OBS and Twitch, YouTube or Kick and holds your stream back as long as you want, with a control panel and an on-stream overlay.

## Download

Get the zip from [Releases](https://github.com/v1niii/DelayDeck/releases/latest), unzip it and run `DelayDeck.exe`. It updates itself.

## Setup

1. In OBS, go to **Settings → Stream**: Service `Custom...`, Server `rtmp://127.0.0.1:1935/live`, and keep your stream key. Turn off Multitrack Video if it's on.
2. In the panel at http://127.0.0.1:3000, pick your platform in **Settings**, or **Test** to try it without going live.
3. Start streaming in OBS and set a delay.

Optional: add `http://127.0.0.1:3000/overlay` as a Browser Source (1920×1080), and `http://127.0.0.1:3000/` as a Custom Browser Dock.

## Build

```
cargo build --release
```

Push a tag like `v0.2.0` to publish a release.
