use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::Serialize;

use crate::config::Mode;
use crate::media::{Frame, Kind};

const RETAIN: Duration = Duration::from_secs(4);
const LEAD: Duration = Duration::from_secs(3);
const MAX_BATCH: usize = 256;
const GAP_MS: i64 = 20;

struct Entry {
    frame: Frame,
    at: Instant,
    epoch: u64,
}

#[derive(Default)]
struct Headers {
    meta: Option<Bytes>,
    video: Option<Bytes>,
    audio: Option<Bytes>,
}

#[derive(Clone, Copy)]
struct Building {
    mode: Mode,
    target: Duration,
    from: Duration,
    started: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Idle,
    Connecting,
    Connected,
    Offline,
}

#[derive(Default)]
struct Timeline {
    offset: i64,
    last: [Option<i64>; 3],
    last_any: Option<i64>,
    resync: bool,
    need_sync: bool,
    need_headers: bool,
    sent_headers: [bool; 3],
    epoch: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct OutFrame {
    pub kind: Kind,
    pub ts: u32,
    pub data: Bytes,
}

pub enum Poll {
    Ready(Vec<OutFrame>),
    Wait(Instant),
    Idle,
    Done,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub target: f64,
    pub delay: f64,
    pub phase: &'static str,
    pub build: Option<&'static str>,
    pub have: Option<f64>,
    pub need: Option<f64>,
    pub remaining: Option<f64>,
    pub buffered: f64,
    pub bytes: usize,
}

pub struct Engine {
    buf: VecDeque<Entry>,
    base: u64,
    cursor: u64,
    bytes: usize,
    delay: Duration,
    target: Duration,
    mode: Mode,
    building: Option<Building>,
    epochs: u64,
    input: Option<(u64, Instant)>,
    video_epoch: Option<u64>,
    headers: VecDeque<(u64, Headers)>,
    link: Link,
    out: Timeline,
}

impl Engine {
    pub fn new(target: Duration, mode: Mode) -> Self {
        Engine {
            buf: VecDeque::new(),
            base: 0,
            cursor: 0,
            bytes: 0,
            delay: target,
            target,
            mode,
            building: None,
            epochs: 0,
            input: None,
            video_epoch: None,
            headers: VecDeque::new(),
            link: Link::Idle,
            out: Timeline::default(),
        }
    }

    pub fn target(&self) -> Duration {
        self.target
    }

    fn end(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    fn entry(&self, idx: u64) -> &Entry {
        &self.buf[(idx - self.base) as usize]
    }

    fn pending(&self) -> bool {
        self.cursor < self.end()
    }

    fn is_sync(&self, e: &Entry) -> bool {
        match e.frame.kind {
            Kind::Video => e.frame.keyframe,
            Kind::Audio => !e.frame.header && self.video_epoch != Some(e.epoch),
            Kind::Data => false,
        }
    }

    fn latest_sync(&self, from: u64, limit: Instant) -> Option<u64> {
        let mut found = None;
        for idx in from..self.end() {
            let e = self.entry(idx);
            if e.at > limit {
                break;
            }
            if self.is_sync(e) {
                found = Some(idx);
            }
        }
        found
    }

    fn jump(&mut self, idx: u64) {
        self.cursor = idx;
        self.out.resync = true;
        self.out.need_sync = false;
    }

    fn seek(&mut self, now: Instant) {
        let Some(limit) = now.checked_sub(self.delay) else { return };
        if let Some(idx) = self.latest_sync(self.cursor, limit)
            && idx > self.cursor {
                self.jump(idx);
            }
    }

    pub fn begin_input(&mut self, now: Instant) {
        self.epochs += 1;
        self.input = Some((self.epochs, now));
        self.headers.push_back((self.epochs, Headers::default()));
        if !self.pending() {
            self.delay = self.target;
            self.building = None;
        }
    }

    pub fn end_input(&mut self) {
        self.input = None;
        if matches!(self.building, Some(b) if b.mode == Mode::Replay) {
            self.building = None;
        }
    }

    pub fn push(&mut self, frame: Frame, now: Instant) -> bool {
        let Some((epoch, _)) = self.input else { return false };
        if frame.header
            && let Some((_, h)) = self.headers.back_mut() {
                let slot = match frame.kind {
                    Kind::Video => &mut h.video,
                    Kind::Audio => &mut h.audio,
                    Kind::Data => &mut h.meta,
                };
                *slot = Some(frame.data.clone());
            }
        if frame.kind == Kind::Video {
            self.video_epoch = Some(epoch);
        }
        let was_idle = !self.pending();
        self.bytes += frame.data.len();
        self.buf.push_back(Entry { frame, at: now, epoch });
        was_idle
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn set_target(&mut self, target: Duration, now: Instant) {
        self.target = target;
        if self.link != Link::Connected || self.out.last_any.is_none() {
            self.delay = target;
            self.building = None;
            return;
        }
        if target <= self.delay {
            self.building = match self.building {
                Some(b) if b.mode == Mode::Pause && target > b.from => Some(Building { target, ..b }),
                _ => None,
            };
            self.delay = target;
            self.seek(now);
            return;
        }
        match self.building {
            Some(b) if b.mode == Mode::Pause => {
                self.building = Some(Building { target, ..b });
                self.delay = target;
            }
            _ => {
                let mode = if self.input.is_some() { self.mode } else { Mode::Pause };
                self.building = Some(Building { mode, target, from: self.delay, started: now });
                if mode == Mode::Pause {
                    self.delay = target;
                }
            }
        }
    }

    fn tick(&mut self, now: Instant) {
        if let Some(b) = self.building {
            match b.mode {
                Mode::Pause => {
                    if now >= b.started + b.target.saturating_sub(b.from) {
                        self.building = None;
                    }
                }
                Mode::Replay => {
                    let ready = now.checked_sub(b.target).and_then(|limit| {
                        if self.buf.front()?.at > limit {
                            return None;
                        }
                        self.latest_sync(self.base, limit)
                    });
                    if let Some(idx) = ready {
                        self.delay = b.target;
                        self.building = None;
                        self.jump(idx);
                    }
                }
            }
        }
        if self.link == Link::Offline {
            while self.pending() && self.entry(self.cursor).at + self.delay <= now {
                self.cursor += 1;
            }
        }
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        let keep = self.delay.max(self.building.map_or(Duration::ZERO, |b| b.target)) + RETAIN;
        if let Some(limit) = now.checked_sub(keep) {
            while self.base < self.cursor && self.buf.front().is_some_and(|e| e.at < limit) {
                let e = self.buf.pop_front().expect("front exists");
                self.bytes -= e.frame.data.len();
                self.base += 1;
            }
        }
        let oldest = self.buf.front().map(|e| e.epoch).or(self.input.map(|(epoch, _)| epoch));
        while self.headers.len() > 1 && self.headers.front().is_some_and(|(epoch, _)| oldest.is_some_and(|o| *epoch < o)) {
            self.headers.pop_front();
        }
    }

    pub fn poll(&mut self, now: Instant) -> Poll {
        self.tick(now);
        let mut out = Vec::new();
        while self.pending() && out.len() < MAX_BATCH {
            let e = self.entry(self.cursor);
            let due = e.at + self.delay;
            if due > now {
                return if out.is_empty() { Poll::Wait(due) } else { Poll::Ready(out) };
            }
            let (kind, ts, header, epoch, sync) = (e.frame.kind, e.frame.ts, e.frame.header, e.epoch, self.is_sync(e));
            let data = e.frame.data.clone();
            self.cursor += 1;

            if self.out.need_sync && !header {
                if !sync {
                    continue;
                }
                self.out.need_sync = false;
                self.out.resync = true;
            }
            if self.out.epoch != Some(epoch) {
                self.out.epoch = Some(epoch);
                self.out.resync = true;
                self.out.need_headers = true;
                self.out.sent_headers = [false; 3];
            }
            let ts = self.stamp(kind, ts);
            if header {
                self.out.sent_headers[kind.idx()] = true;
            } else if self.out.need_headers {
                self.out.need_headers = false;
                self.emit_headers(epoch, ts, &mut out);
            }
            out.push(OutFrame { kind, ts, data });
        }
        if !out.is_empty() {
            Poll::Ready(out)
        } else if self.input.is_none() && !self.pending() {
            Poll::Done
        } else {
            Poll::Idle
        }
    }

    fn stamp(&mut self, kind: Kind, ts: u32) -> u32 {
        let o = &mut self.out;
        if o.resync {
            o.resync = false;
            let base = o.last_any.map_or(0, |t| t + GAP_MS);
            o.offset = base - ts as i64;
        }
        let mut t = (ts as i64 + o.offset).max(0);
        if let Some(prev) = o.last[kind.idx()] {
            t = t.max(prev);
        }
        o.last[kind.idx()] = Some(t);
        o.last_any = Some(o.last_any.map_or(t, |a| a.max(t)));
        t as u32
    }

    fn emit_headers(&mut self, epoch: u64, ts: u32, out: &mut Vec<OutFrame>) {
        let Some((_, h)) = self.headers.iter().find(|(e, _)| *e == epoch) else { return };
        for (kind, data) in [(Kind::Data, &h.meta), (Kind::Video, &h.video), (Kind::Audio, &h.audio)] {
            if self.out.sent_headers[kind.idx()] {
                continue;
            }
            if let Some(data) = data {
                out.push(OutFrame { kind, ts, data: data.clone() });
                let last = &mut self.out.last[kind.idx()];
                *last = Some(last.map_or(ts as i64, |l| l.max(ts as i64)));
            }
        }
    }

    pub fn set_link(&mut self, link: Link) {
        self.link = link;
    }

    pub fn output_connected(&mut self, now: Instant) {
        self.link = Link::Connected;
        self.out = Timeline { resync: true, need_sync: true, need_headers: true, ..Timeline::default() };
        self.seek(now);
    }

    pub fn output_finished(&mut self) {
        self.link = Link::Idle;
        self.out = Timeline::default();
        if self.input.is_none() {
            self.delay = self.target;
            self.building = None;
        }
    }

    pub fn wants_output(&self, now: Instant) -> bool {
        self.pending() && self.entry(self.cursor).at + self.delay <= now + LEAD
    }

    pub fn snapshot(&mut self, now: Instant) -> Snapshot {
        self.tick(now);
        let secs = |d: Duration| (d.as_secs_f64() * 10.0).round() / 10.0;
        let held = self.buf.front().map_or(Duration::ZERO, |e| now.saturating_duration_since(e.at));
        let mut s = Snapshot {
            target: secs(self.target),
            delay: secs(self.delay),
            phase: "idle",
            build: None,
            have: None,
            need: None,
            remaining: None,
            buffered: secs(held),
            bytes: self.bytes,
        };
        if let Some(b) = self.building {
            let (kind, have) = match b.mode {
                Mode::Replay => ("replay", held),
                Mode::Pause => ("pause", b.from + now.saturating_duration_since(b.started)),
            };
            s.phase = "building";
            s.build = Some(kind);
            s.have = Some(secs(have.min(b.target)));
            s.need = Some(secs(b.target));
        } else if let Some((epoch, started)) = self.input {
            let waited = now.saturating_duration_since(started);
            if self.out.epoch != Some(epoch) && waited < self.delay {
                s.phase = "building";
                s.build = Some("start");
                s.have = Some(secs(waited));
                s.need = Some(secs(self.delay));
            } else {
                s.phase = if self.delay.is_zero() { "live" } else { "delayed" };
            }
        } else if self.pending() {
            s.phase = "draining";
            let last = self.buf.back().map_or(now, |e| e.at);
            s.remaining = Some(secs((last + self.delay).saturating_duration_since(now)));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: u64 = 10;

    struct Sim {
        e: Engine,
        t0: Instant,
        now_ms: u64,
        live: bool,
        sent: Vec<(u64, Kind, u32, u32)>,
    }

    impl Sim {
        fn new(delay_s: u64, mode: Mode) -> Sim {
            let mut e = Engine::new(Duration::from_secs(delay_s), mode);
            let t0 = Instant::now();
            e.begin_input(t0);
            e.output_connected(t0);
            Sim { e, t0, now_ms: 0, live: true, sent: Vec::new() }
        }

        fn now(&self) -> Instant {
            self.t0 + Duration::from_millis(self.now_ms)
        }

        fn frame(kind: Kind, ts: u32, keyframe: bool, header: bool) -> Frame {
            Frame { kind, ts, data: Bytes::from(ts.to_be_bytes().to_vec()), keyframe, header }
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms / STEP {
                self.now_ms += STEP;
                let now = self.now();
                let ts = self.now_ms as u32;
                if self.live {
                    if self.now_ms.is_multiple_of(20) {
                        self.e.push(Self::frame(Kind::Audio, ts, false, false), now);
                    }
                    if self.now_ms.is_multiple_of(40) {
                        self.e.push(Self::frame(Kind::Video, ts, self.now_ms % 1000 == 40, false), now);
                    }
                }
                self.drain();
            }
        }

        fn drain(&mut self) -> bool {
            loop {
                match self.e.poll(self.now()) {
                    Poll::Ready(frames) => {
                        for f in frames {
                            let input = u32::from_be_bytes(f.data[..4].try_into().unwrap());
                            self.sent.push((self.now_ms, f.kind, f.ts, input));
                        }
                    }
                    Poll::Done => return true,
                    _ => return false,
                }
            }
        }

        fn video(&self) -> Vec<(u64, u32, u32)> {
            self.sent.iter().filter(|s| s.1 == Kind::Video).map(|s| (s.0, s.2, s.3)).collect()
        }

        fn assert_monotonic(&self) {
            for kind in [Kind::Audio, Kind::Video] {
                let ts: Vec<u32> = self.sent.iter().filter(|s| s.1 == kind).map(|s| s.2).collect();
                assert!(ts.windows(2).all(|w| w[0] <= w[1]), "{kind:?} timestamps went backwards: {ts:?}");
            }
        }
    }

    #[test]
    fn zero_delay_passes_frames_through() {
        let mut sim = Sim::new(0, Mode::Replay);
        sim.run(3000);
        let v = sim.video();
        assert_eq!(v.first().map(|f| f.2), Some(40), "starts at the first keyframe");
        assert!(v.iter().all(|&(wall, _, input)| wall == input as u64), "no added latency");
        sim.assert_monotonic();
    }

    #[test]
    fn delay_holds_frames() {
        let mut sim = Sim::new(5, Mode::Replay);
        sim.run(4900);
        assert!(sim.sent.is_empty());
        sim.run(3000);
        let v = sim.video();
        assert!(v.iter().all(|&(wall, _, input)| wall == input as u64 + 5000));
        assert_eq!(sim.sent[0].2, 0, "outgoing timeline starts at zero");
    }

    #[test]
    fn skip_to_live_lands_on_latest_keyframe() {
        let mut sim = Sim::new(5, Mode::Replay);
        sim.run(10_000);
        let before = sim.sent.len();
        sim.e.set_target(Duration::ZERO, sim.now());
        sim.drain();
        let burst: Vec<_> = sim.sent[before..].iter().filter(|s| s.1 == Kind::Video).collect();
        assert_eq!(burst[0].3, 9040, "jumped to the newest keyframe");
        sim.run(2000);
        let tail = sim.video();
        assert!(tail.last().is_some_and(|&(wall, _, input)| wall == input as u64), "caught up to live");
        sim.assert_monotonic();
    }

    #[test]
    fn replay_increase_keeps_streaming_then_jumps_back() {
        let mut sim = Sim::new(0, Mode::Replay);
        sim.run(10_000);
        sim.e.set_target(Duration::from_secs(8), sim.now());
        let snap = sim.e.snapshot(sim.now());
        assert_eq!((snap.phase, snap.build), ("building", Some("replay")));
        sim.run(3000);
        let v = sim.video();
        assert!(v.iter().any(|&(wall, _, input)| wall >= 12_000 && wall == input as u64));
        sim.run(3000);
        let v = sim.video();
        let back = v.windows(2).find(|w| w[1].2 < w[0].2).expect("output jumped back");
        assert!(back[0].2 - back[1].2 >= 7000, "replayed roughly the delay difference");
        let last = v.last().unwrap();
        assert_eq!(last.0 - last.2 as u64, 8000, "settled at the new delay");
        sim.assert_monotonic();
    }

    #[test]
    fn pause_increase_stalls_without_repeating() {
        let mut sim = Sim::new(0, Mode::Pause);
        sim.run(5000);
        sim.e.set_target(Duration::from_secs(3), sim.now());
        let before = sim.video().len();
        sim.run(2900);
        assert_eq!(sim.video().len(), before, "output held while the buffer fills");
        sim.run(2000);
        let v = sim.video();
        assert!(v.windows(2).all(|w| w[1].2 > w[0].2), "nothing repeated or skipped backwards");
        assert_eq!(v.last().map(|f| f.0 - f.2 as u64), Some(3000));
        sim.assert_monotonic();
    }

    #[test]
    fn drains_after_obs_stops() {
        let mut sim = Sim::new(2, Mode::Replay);
        sim.run(5000);
        sim.live = false;
        sim.e.end_input();
        assert_eq!(sim.e.snapshot(sim.now()).phase, "draining");
        sim.run(1500);
        assert!(!sim.drain());
        sim.run(600);
        assert!(sim.drain(), "done once the buffer is empty");
        assert_eq!(sim.video().last().map(|f| f.2), Some(5000));
    }

    #[test]
    fn reconnect_resends_headers_before_keyframe() {
        let mut sim = Sim::new(0, Mode::Replay);
        let now = sim.now();
        sim.e.push(Sim::frame(Kind::Data, 0, false, true), now);
        sim.e.push(Sim::frame(Kind::Video, 0, false, true), now);
        sim.e.push(Sim::frame(Kind::Audio, 0, false, true), now);
        sim.run(2500);
        sim.e.set_link(Link::Offline);
        sim.run(1000);
        sim.e.output_connected(sim.now());
        let before = sim.sent.len();
        sim.run(1000);
        let resumed: Vec<_> = sim.sent[before..].iter().map(|s| s.1).collect();
        assert_eq!(&resumed[..4], &[Kind::Data, Kind::Video, Kind::Audio, Kind::Video], "headers then keyframe");
        let first_video = sim.sent[before..].iter().find(|s| s.1 == Kind::Video && s.3 != 0).unwrap();
        assert_eq!(first_video.3 % 1000, 40, "resumed on a keyframe");
    }

    #[test]
    fn offline_output_keeps_memory_bounded() {
        let mut sim = Sim::new(1, Mode::Replay);
        sim.e.set_link(Link::Offline);
        sim.run(20_000);
        let snap = sim.e.snapshot(sim.now());
        assert!(snap.buffered <= 1.0 + RETAIN.as_secs_f64() + 0.1, "held {}s", snap.buffered);
    }
}
