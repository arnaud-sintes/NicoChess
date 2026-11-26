//! Figures about the way a picture goes from the camera to the tracker: how long the tracker takes for one look, how
//! old a picture is when the tracker gets to it, how many pictures arrived without being looked at, and the longest
//! wait between two looks. They tell a tracker that fell behind (lag, pictures that were never looked at) from one
//! that decided badly on every picture it saw, which the pictures and decisions of a recording alone cannot.
//!
//! The figures are kept for the whole game (written at its end, in the `closed` line of the recording) and for the
//! last stretch (printed every [`REPORT_EVERY`] while playing). The engine calls this with its lock held, so every
//! call is a few additions: nothing here waits, allocates much, or writes.

use crate::analysis::Backlog;
use crate::tags::Tag;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// How often the console gets a line with the figures of the last stretch.
pub const REPORT_EVERY: Duration = Duration::from_secs(30);
/// A picture older than this when the tracker starts on it, or a wait longer than [`BEHIND_GAP`] between two looks, counts
/// as the tracker being behind.
pub const BEHIND_AGE: Duration = Duration::from_millis(500);
pub const BEHIND_GAP: Duration = Duration::from_millis(300);

const BUCKETS: usize = 64;
/// The upper limit of the first bucket, in milliseconds, and the ratio between one bucket and the next: 64 buckets of
/// 30% reach 100 minutes, so a figure is told to within 30% (the maximum and the mean are exact).
const FIRST_MS: f32 = 0.1;
const RATIO: f32 = 1.3;

/// What a series of durations (in milliseconds) came to, in a fixed amount of memory.
#[derive(Clone)]
pub struct Hist {
    counts: [u32; BUCKETS],
    n: u64,
    sum: f64,
    max: f32,
}

impl Default for Hist {
    fn default() -> Hist {
        Hist { counts: [0; BUCKETS], n: 0, sum: 0.0, max: 0.0 }
    }
}

impl Hist {
    pub fn add(&mut self, ms: f32) {
        let ms = if ms.is_finite() { ms.max(0.0) } else { 0.0 };
        let bucket = if ms <= FIRST_MS { 0 } else { ((ms / FIRST_MS).ln() / RATIO.ln()).ceil() as usize };
        self.counts[bucket.min(BUCKETS - 1)] += 1;
        self.n += 1;
        self.sum += f64::from(ms);
        self.max = self.max.max(ms);
    }

    pub fn count(&self) -> u64 {
        self.n
    }

    pub fn mean(&self) -> f32 {
        if self.n == 0 { 0.0 } else { (self.sum / self.n as f64) as f32 }
    }

    pub fn max(&self) -> f32 {
        self.max
    }

    /// The figure that 95 of 100 are not above, to within one bucket (never above the maximum).
    pub fn p95(&self) -> f32 {
        if self.n == 0 {
            return 0.0;
        }
        let wanted = (self.n as f64 * 0.95).ceil() as u64;
        let mut seen = 0u64;
        for (i, count) in self.counts.iter().enumerate() {
            seen += u64::from(*count);
            if seen >= wanted {
                return (FIRST_MS * RATIO.powi(i as i32)).min(self.max);
            }
        }
        self.max
    }

    /// `{"mean":..,"p95":..,"max":..}`, or null when nothing was counted.
    pub fn json(&self) -> Value {
        if self.n == 0 {
            return Value::Null;
        }
        json!({"mean": round1(self.mean()), "p95": round1(self.p95()), "max": round1(self.max)})
    }

    /// "12 ms (p95 30, max 80)".
    fn text(&self) -> String {
        format!("{:.0} ms (p95 {:.0}, max {:.0})", self.mean(), self.p95(), self.max)
    }
}

fn round1(x: f32) -> f64 {
    (f64::from(x) * 10.0).round() / 10.0
}

fn ms(d: Duration) -> f32 {
    d.as_secs_f32() * 1000.0
}

/// What is known about the pictures of one view (a camera, or the height picture of the depth camera).
#[derive(Clone, Default)]
pub struct ViewFigures {
    /// Pictures that arrived while playing, and the ones of them that were handed to the tracker.
    pub received: u64,
    pub fed: u64,
    /// Pictures that were never handed to the tracker: the gaps in the numbers of the pictures it was given. With two
    /// cameras the tracker looks at the latest picture of each whenever either one has a new one, so a picture of the
    /// camera that did not start the look is still looked at, a moment later: only the pictures that were replaced by a
    /// newer one in between are counted here. (It is not known why: the pace of `track_interval`, a tracker busy with
    /// the picture before, or a camera not usable at the time.)
    pub skipped: u64,
    /// How long the capture thread waited for the picture inside the camera's `next_frame`: close to nothing means that
    /// the picture was already there, so the program was not keeping up with the camera.
    pub wait: Hist,
    /// The time between two pictures as the camera made them: a gap of several pictures there means that the camera or
    /// its driver lost pictures before the program saw them.
    pub capture_gap: Hist,
    /// How old a picture was when the tracker started on it.
    pub age: Hist,
    /// The time between two looks of the tracker at this view: its maximum is the longest wait.
    pub fed_gap: Hist,
    /// How far in time (in ms, either way) the picture of this view that went with a look was from the picture that
    /// began the look: the pairing skew. Only for views whose picture was a companion, not the one that began the look.
    pub skew: Hist,
    /// Looks in which this view took part and was given no picture, because none was captured near enough in time.
    pub unpaired: u64,
    /// Pictures that a glance found to be the same as the one before them, and pictures with a hand or an arm over the
    /// board (see the module `tags`): information only, the tracker looks at them all the same.
    pub duplicates: u64,
    pub occluded: u64,
}

/// The figures of a stretch of play.
#[derive(Clone)]
pub struct Sets {
    /// Times the tracker looked, how many of them were late (see [`BEHIND_AGE`]), how long one look took, and how long
    /// the analysis thread waited for the engine's lock to publish what the look found.
    pub feeds: u64,
    pub behind: u64,
    /// Looks that were skipped because the analysis was behind and the board was quiet (see `analysis::Thin`): the
    /// pictures were kept, the tracker was told that one more still picture went by.
    pub thinned: u64,
    pub step: Hist,
    pub lock: Hist,
    /// At each look: how many pictures waited for the analysis, and how old (in ms, since its capture) the oldest was.
    pub waiting: Hist,
    pub oldest: Hist,
    pub views: Vec<ViewFigures>,
}

impl Sets {
    fn new(views: usize) -> Sets {
        Sets {
            feeds: 0,
            behind: 0,
            thinned: 0,
            step: Hist::default(),
            lock: Hist::default(),
            waiting: Hist::default(),
            oldest: Hist::default(),
            views: vec![ViewFigures::default(); views],
        }
    }
}

/// What the tracker was given for one view this time.
pub struct Fed {
    pub view: usize,
    /// The number of the picture among those of its view (a gap in the numbers is pictures that were not looked at).
    pub seq: u64,
    /// When the camera made the picture, and how long the capture thread waited for it (not known for the depth camera).
    pub captured: Instant,
    pub waited: Option<Duration>,
    /// When it was captured, counted from the picture that began the look (in ms, negative before it); none for the picture
    /// that began it.
    pub skew_ms: Option<f32>,
}

/// What a look of the tracker was like, for the recording.
pub struct FeedNote {
    pub step_ms: f32,
    pub lock_ms: f32,
    pub views: Vec<ViewNote>,
    /// Views that took part and were given no picture (none was near enough in time).
    pub unpaired: Vec<usize>,
    /// The look was skipped (see `analysis::Thin`).
    pub thin: bool,
}

pub struct ViewNote {
    pub view: usize,
    pub age_ms: f32,
    pub wait_ms: Option<f32>,
    pub capture_gap_ms: Option<f32>,
    /// Pictures of this view that came since the one the tracker looked at the time before.
    pub skipped: u64,
    pub since_last_ms: Option<f32>,
    pub skew_ms: Option<f32>,
}

impl FeedNote {
    pub fn json(&self) -> Value {
        let views: Vec<Value> = self
            .views
            .iter()
            .map(|v| {
                let mut o = json!({"cam": v.view, "age_ms": round1(v.age_ms), "skipped": v.skipped});
                if let Some(x) = v.wait_ms {
                    o["wait_ms"] = json!(round1(x));
                }
                if let Some(x) = v.capture_gap_ms {
                    o["gap_ms"] = json!(round1(x));
                }
                if let Some(x) = v.since_last_ms {
                    o["since_last_ms"] = json!(round1(x));
                }
                if let Some(x) = v.skew_ms {
                    o["skew_ms"] = json!(round1(x));
                }
                o
            })
            .collect();
        let mut note = json!({"step_ms": round1(self.step_ms), "lock_ms": round1(self.lock_ms), "views": views});
        if !self.unpaired.is_empty() {
            note["unpaired"] = json!(self.unpaired);
        }
        if self.thin {
            note["thin"] = json!(true);
        }
        note
    }
}

#[derive(Clone, Default)]
struct Tracking {
    last_captured: Option<Instant>,
    /// The time between the last two pictures as the camera made them.
    capture_gap: Option<Duration>,
    last_fed_seq: Option<u64>,
    last_fed_at: Option<Instant>,
}

/// The figures of the game being played: for the whole game, and for the stretch since the last console line.
pub struct Pipe {
    total: Sets,
    window: Sets,
    window_began: Instant,
    tracking: Vec<Tracking>,
}

impl Pipe {
    pub fn new(views: usize, now: Instant) -> Pipe {
        Pipe { total: Sets::new(views), window: Sets::new(views), window_began: now, tracking: vec![Tracking::default(); views] }
    }

    /// A picture came from a camera while playing.
    pub fn arrival(&mut self, view: usize, captured: Instant, waited: Duration) {
        let Some(track) = self.tracking.get_mut(view) else { return };
        let gap = track.last_captured.map(|last| captured.saturating_duration_since(last));
        track.last_captured = Some(captured);
        track.capture_gap = gap;
        for sets in [&mut self.total, &mut self.window] {
            let figures = &mut sets.views[view];
            figures.received += 1;
            figures.wait.add(ms(waited));
            if let Some(gap) = gap {
                figures.capture_gap.add(ms(gap));
            }
        }
    }

    /// The tracker looked at `pictures`, starting at `started`, after `lock` waiting for the engine's lock, and took
    /// `step` to do it.
    pub fn fed(&mut self, started: Instant, lock: Duration, step: Duration, pictures: &[Fed], unpaired: &[usize]) -> FeedNote {
        let mut note = FeedNote { step_ms: ms(step), lock_ms: ms(lock), views: Vec::new(), unpaired: unpaired.to_vec(), thin: false };
        for &view in unpaired {
            for sets in [&mut self.total, &mut self.window] {
                if let Some(figures) = sets.views.get_mut(view) {
                    figures.unpaired += 1;
                }
            }
        }
        let mut behind = false;
        for picture in pictures {
            let Some(track) = self.tracking.get_mut(picture.view) else { continue };
            let age = started.saturating_duration_since(picture.captured);
            let since_last = track.last_fed_at.map(|at| started.saturating_duration_since(at));
            let skipped = track.last_fed_seq.map_or(0, |last| picture.seq.saturating_sub(last + 1));
            track.last_fed_seq = Some(picture.seq);
            track.last_fed_at = Some(started);
            behind |= age > BEHIND_AGE || since_last.is_some_and(|g| g > BEHIND_GAP);
            for sets in [&mut self.total, &mut self.window] {
                let figures = &mut sets.views[picture.view];
                figures.fed += 1;
                figures.skipped += skipped;
                figures.age.add(ms(age));
                if let Some(gap) = since_last {
                    figures.fed_gap.add(ms(gap));
                }
                if let Some(skew) = picture.skew_ms {
                    figures.skew.add(skew.abs());
                }
            }
            note.views.push(ViewNote {
                view: picture.view,
                age_ms: ms(age),
                wait_ms: picture.waited.map(ms),
                capture_gap_ms: track.capture_gap.map(ms),
                skipped,
                since_last_ms: since_last.map(ms),
                skew_ms: picture.skew_ms,
            });
        }
        for sets in [&mut self.total, &mut self.window] {
            sets.feeds += 1;
            sets.behind += u64::from(behind);
            sets.step.add(ms(step));
            sets.lock.add(ms(lock));
        }
        note
    }

    /// What a glance said of the pictures of this look, and whether the look was skipped (see `analysis::Thin`).
    pub fn tagged(&mut self, tags: &[(usize, Tag)], thinned: bool) {
        for sets in [&mut self.total, &mut self.window] {
            sets.thinned += u64::from(thinned);
            for &(view, tag) in tags {
                if let Some(figures) = sets.views.get_mut(view) {
                    figures.duplicates += u64::from(tag.duplicate);
                    figures.occluded += u64::from(tag.occluded);
                }
            }
        }
    }

    /// How many pictures waited for the analysis at this look, and how old the oldest was.
    pub fn backlog(&mut self, backlog: &Backlog) {
        for sets in [&mut self.total, &mut self.window] {
            sets.waiting.add(backlog.waiting as f32);
            sets.oldest.add(ms(backlog.oldest));
        }
    }

    /// What the whole game came to, for the `closed` line of the recording.
    pub fn summary(&self) -> Value {
        let mut views = serde_json::Map::new();
        for (i, v) in self.total.views.iter().enumerate() {
            if v.received == 0 && v.fed == 0 && v.unpaired == 0 {
                continue;
            }
            views.insert(
                format!("cam{i}"),
                json!({
                    "received": v.received, "fed": v.fed, "skipped": v.skipped,
                    "age_ms": v.age.json(), "wait_ms": v.wait.json(), "capture_gap_ms": v.capture_gap.json(),
                    "longest_gap_ms": if v.fed_gap.count() > 0 { json!(round1(v.fed_gap.max())) } else { Value::Null },
                    "pair_skew_ms": v.skew.json(), "unpaired": v.unpaired,
                    "duplicates": v.duplicates, "occluded": v.occluded,
                }),
            );
        }
        json!({
            "feeds": self.total.feeds, "behind": self.total.behind, "thinned": self.total.thinned,
            "step_ms": self.total.step.json(), "lock_ms": self.total.lock.json(), "views": views,
            "waiting": self.total.waiting.json(), "oldest_waiting_ms": self.total.oldest.json(),
        })
    }

    /// A line for the console with the figures since the last one, when it is time for it (and something happened).
    pub fn report(&mut self, now: Instant) -> Option<String> {
        let span = now.saturating_duration_since(self.window_began);
        if span < REPORT_EVERY {
            return None;
        }
        let sets = std::mem::replace(&mut self.window, Sets::new(self.tracking.len()));
        self.window_began = now;
        if sets.feeds == 0 {
            return None;
        }
        let mut line = format!(
            "pipeline, last {:.0} s: {} looks, tracker {}, waiting for the lock {}, pictures waiting for the analysis: {:.0} (p95 {:.0}, max {:.0}), the oldest {}{}",
            span.as_secs_f32(),
            sets.feeds,
            sets.step.text(),
            sets.lock.text(),
            sets.waiting.mean(),
            sets.waiting.p95(),
            sets.waiting.max(),
            sets.oldest.text(),
            if sets.behind > 0 {
                format!(
                    ", BEHIND {} times (a picture over {} ms old or {} ms between looks)",
                    sets.behind,
                    BEHIND_AGE.as_millis(),
                    BEHIND_GAP.as_millis()
                )
            } else {
                String::new()
            },
        );
        if sets.thinned > 0 {
            line.push_str(&format!(
                ", {} looks skipped (the board was quiet and the same, the analysis behind; the pictures are kept)",
                sets.thinned
            ));
        }
        for (i, v) in sets.views.iter().enumerate() {
            if v.received == 0 && v.fed == 0 && v.unpaired == 0 {
                continue;
            }
            let name = if i == 2 { "depth".to_string() } else { format!("camera {}", i + 1) };
            line.push_str(&format!(
                "; {name}: {} came, {} looked at, {} never looked at, age {}, longest wait {:.0} ms",
                v.received,
                v.fed,
                v.skipped,
                v.age.text(),
                v.fed_gap.max()
            ));
            if v.skew.count() > 0 || v.unpaired > 0 {
                line.push_str(&format!(", {} paired {} away in time, {} looks without it", v.skew.count(), v.skew.text(), v.unpaired));
            }
            if v.occluded > 0 {
                line.push_str(&format!(", {} with a hand or arm over the board", v.occluded));
            }
        }
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_histogram_tells_mean_p95_and_max() {
        let mut h = Hist::default();
        for i in 1..=100 {
            h.add(i as f32);
        }
        assert_eq!(h.count(), 100);
        assert!((h.mean() - 50.5).abs() < 0.01);
        assert_eq!(h.max(), 100.0);
        // Told to within one bucket (30%): 95 is the 95th of 100 values.
        let p = h.p95();
        assert!((95.0..=95.0 * 1.31).contains(&p) || p == 100.0, "{p}");
        assert!(Hist::default().json().is_null());
        // A huge or odd value does not break it.
        h.add(f32::NAN);
        h.add(1e12);
        assert_eq!(h.count(), 102);
    }

    #[test]
    fn looks_count_what_came_what_was_skipped_and_how_old_it_was() {
        let t0 = Instant::now();
        let mut pipe = Pipe::new(3, t0);
        // Four pictures of camera 0 come; the tracker looks at the first and the fourth.
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        pipe.arrival(0, at(0), Duration::from_millis(30));
        let a = pipe.fed(
            at(10),
            Duration::from_millis(1),
            Duration::from_millis(7),
            &[Fed { view: 0, seq: 1, captured: at(0), waited: Some(Duration::from_millis(30)), skew_ms: None }],
            &[],
        );
        assert_eq!((a.views[0].skipped, a.views[0].since_last_ms), (0, None));
        assert!((a.views[0].age_ms - 10.0).abs() < 0.01 && (a.step_ms - 7.0).abs() < 0.01);
        for k in 1..=3 {
            pipe.arrival(0, at(40 * k), Duration::from_millis(2));
        }
        let b = pipe.fed(
            at(130),
            Duration::ZERO,
            Duration::from_millis(50),
            &[Fed { view: 0, seq: 4, captured: at(120), waited: None, skew_ms: None }],
            &[],
        );
        assert_eq!(b.views[0].skipped, 2, "pictures 2 and 3 were not looked at");
        assert!((b.views[0].since_last_ms.unwrap() - 120.0).abs() < 0.01);
        assert!((b.views[0].capture_gap_ms.unwrap() - 40.0).abs() < 0.01);

        let s = pipe.summary();
        assert_eq!((s["feeds"].as_u64(), s["behind"].as_u64()), (Some(2), Some(0)));
        let cam0 = &s["views"]["cam0"];
        assert_eq!((cam0["received"].as_u64(), cam0["fed"].as_u64()), (Some(4), Some(2)));
        assert_eq!(cam0["skipped"].as_u64(), Some(2));
        assert_eq!(cam0["longest_gap_ms"].as_f64(), Some(120.0));
        assert!(s["views"].get("cam1").is_none(), "a view that gave nothing is left out");
    }

    #[test]
    fn how_far_in_time_the_pictures_of_a_look_were_from_each_other_is_counted_and_a_view_with_no_picture_is_too() {
        let t0 = Instant::now();
        let mut pipe = Pipe::new(3, t0);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let pictures = |skew: f32| {
            vec![
                Fed { view: 0, seq: 1, captured: at(100), waited: None, skew_ms: None },
                Fed {
                    view: 1,
                    seq: 1,
                    captured: at(100) + Duration::from_secs_f32(skew.abs() / 1000.0),
                    waited: None,
                    skew_ms: Some(skew),
                },
            ]
        };
        let note = pipe.fed(at(110), Duration::ZERO, Duration::from_millis(2), &pictures(15.0), &[]);
        assert_eq!((note.views[0].skew_ms, note.views[1].skew_ms), (None, Some(15.0)));
        let line = note.json();
        assert_eq!(line["views"][1]["skew_ms"].as_f64(), Some(15.0));
        assert!(line.get("unpaired").is_none());
        let mut next = pictures(-25.0);
        next[0].seq = 2;
        next[1].seq = 2;
        let note = pipe.fed(at(160), Duration::ZERO, Duration::from_millis(2), &next, &[2]);
        assert_eq!(note.json()["unpaired"], serde_json::json!([2]));
        let summary = pipe.summary();
        let cam1 = &summary["views"]["cam1"];
        assert_eq!(cam1["unpaired"].as_u64(), Some(0));
        assert_eq!(cam1["pair_skew_ms"]["max"].as_f64(), Some(25.0), "the skew is counted either way");
        assert!((cam1["pair_skew_ms"]["mean"].as_f64().unwrap() - 20.0).abs() < 0.2, "{cam1}");
        assert!(summary["views"]["cam0"]["pair_skew_ms"].is_null(), "the picture that began a look has no skew");
        assert_eq!(pipe.total.views[2].unpaired, 1);
    }

    #[test]
    fn a_picture_that_is_old_or_a_long_wait_counts_as_behind_and_the_console_line_says_so() {
        let t0 = Instant::now();
        let mut pipe = Pipe::new(3, t0);
        let late = t0 + Duration::from_millis(900);
        pipe.arrival(0, t0, Duration::ZERO);
        pipe.fed(
            late,
            Duration::ZERO,
            Duration::from_millis(5),
            &[Fed { view: 0, seq: 1, captured: t0, waited: None, skew_ms: None }],
            &[],
        );
        assert_eq!(pipe.summary()["behind"], 1, "0.9 s old");
        assert!(pipe.report(t0 + Duration::from_secs(10)).is_none(), "not yet");
        let line = pipe.report(t0 + REPORT_EVERY).expect("it is time");
        assert!(line.contains("BEHIND 1 times") && line.contains("camera 1"), "{line}");
        // The next stretch starts empty: nothing to say, and the whole-game figures are kept.
        assert!(pipe.report(t0 + REPORT_EVERY * 2).is_none());
        assert_eq!(pipe.summary()["feeds"], 1);
    }
}
