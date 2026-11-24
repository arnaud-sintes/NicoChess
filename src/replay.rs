//! Replay of a recording (`--replay <folder>`): the recorded pictures go through the tracker again, as they
//! did during the game, and what it finds is set beside what happened then. This is for looking into a game
//! that went wrong, and for trying other tuning values (`--max-excess 80`, ...) on real pictures.
//!
//! What was recorded as having happened is taken as the truth: a move the program found, a move the person
//! typed, a position set by undo. Where the replay is behind (it has not found a move that was played) it is
//! put on the real position, as a person would by typing the move, and counted: that count is the number of
//! moves the tuning would have made someone type. A move the replay finds that leads to a position the game
//! never had is counted as wrong. A move the person undid after the program had found it still counts as
//! played: look at such a game by hand.
//!
//! The pictures are fed at the times they were taken, so the replay is as fast as the computer, not the game.

use crate::game::{fen_of, parse_fen, san_of};
use crate::record::{FORMAT, SessionMeta};
use crate::tracker::{Detection, Status, Tracker, TrackerConfig};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use shakmaty::Move;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Tuning values given on the command line, which take the place of the recorded ones.
#[derive(Clone, Debug, Default)]
pub struct TrackerOverrides {
    pub stable_frames: Option<usize>,
    pub stable_time: Option<f32>,
    pub motion_span: Option<f32>,
    pub motion_threshold: Option<f32>,
    pub max_excess: Option<f32>,
    pub margin: Option<f32>,
    pub max_plies: Option<usize>,
    pub soft_margin: Option<f32>,
}

impl TrackerOverrides {
    /// `config` with the values that were given.
    pub fn apply(&self, config: TrackerConfig) -> TrackerConfig {
        TrackerConfig {
            stable_frames: self.stable_frames.unwrap_or(config.stable_frames),
            stable_time: self.stable_time.unwrap_or(config.stable_time),
            motion_span: self.motion_span.unwrap_or(config.motion_span),
            motion_threshold: self.motion_threshold.unwrap_or(config.motion_threshold),
            max_excess: self.max_excess.unwrap_or(config.max_excess),
            margin: self.margin.unwrap_or(config.margin),
            max_plies: self.max_plies.unwrap_or(config.max_plies),
            soft_margin: self.soft_margin.unwrap_or(config.soft_margin),
        }
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.stable_frames {
            parts.push(format!("stable_frames={v}"));
        }
        if let Some(v) = self.stable_time {
            parts.push(format!("stable_time={v}"));
        }
        if let Some(v) = self.motion_span {
            parts.push(format!("motion_span={v}"));
        }
        if let Some(v) = self.motion_threshold {
            parts.push(format!("motion_threshold={v}"));
        }
        if let Some(v) = self.max_excess {
            parts.push(format!("max_excess={v}"));
        }
        if let Some(v) = self.margin {
            parts.push(format!("margin={v}"));
        }
        if let Some(v) = self.max_plies {
            parts.push(format!("max_plies={v}"));
        }
        if let Some(v) = self.soft_margin {
            parts.push(format!("soft_margin={v}"));
        }
        parts.join(", ")
    }
}

/// The figures of one replay.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// Pictures fed to the tracker.
    pub pictures: usize,
    /// During the game: moves the program found by itself (one count per detection of one or more moves), moves
    /// typed by hand, and undos.
    pub live_found: usize,
    pub live_typed: usize,
    pub live_undone: usize,
    /// In the replay: detections that lead to a position the game had, and detections that do not.
    pub found: usize,
    pub wrong: usize,
    /// Places (a move found or typed during the game) where the game was in a position that the replay had not
    /// reached: it was put there.
    pub behind: usize,
    /// Of the moves typed by hand during the game, how many the replay had found by itself.
    pub typed_found: usize,
}

pub struct Report {
    pub text: String,
    pub summary: Summary,
}

/// A line of `events.jsonl`.
struct Event {
    t: f64,
    kind: String,
    data: Value,
}

impl Event {
    fn text(&self, key: &str) -> &str {
        self.data[key].as_str().unwrap_or("")
    }
}

/// Reads the events; a line that cannot be read (the last one, when the power was cut) is skipped.
fn read_events(folder: &Path) -> Result<Vec<Event>> {
    let path = folder.join("events.jsonl");
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut events = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(data) = serde_json::from_str::<Value>(line) else { continue };
        let (Some(t), Some(kind)) = (data["t"].as_f64(), data["type"].as_str()) else { continue };
        events.push(Event { t, kind: kind.to_string(), data: data.clone() });
    }
    Ok(events)
}

/// Pictures that were given to the tracker together (one per camera, with the same `feed` number) become one event
/// with a list `files` of `{cam, file}`, so that the replay gives them together. A recording from before there were
/// two cameras has no numbers: each picture is a time by itself, of camera 0.
fn group_pictures(events: Vec<Event>) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    let mut last_feed: Option<u64> = None;
    for mut event in events {
        if event.kind != "frame" {
            last_feed = None;
            out.push(event);
            continue;
        }
        let file = serde_json::json!({"cam": event.data["cam"].as_u64().unwrap_or(0), "file": event.text("file")});
        let feed = event.data["feed"].as_u64();
        if let (Some(previous), Some(number), Some(last)) = (out.last_mut(), feed, last_feed)
            && number == last
            && let Some(list) = previous.data["files"].as_array_mut()
        {
            list.push(file);
            continue;
        }
        event.data["files"] = serde_json::json!([file]);
        last_feed = feed;
        out.push(event);
    }
    out
}

/// The position without the move counters: what makes two positions the same one.
fn key_of(fen: &str) -> String {
    fen.split(' ').take(4).collect::<Vec<_>>().join(" ")
}

fn sans_of(before: &shakmaty::Chess, moves: &[Move]) -> String {
    use shakmaty::Position;
    let mut position = before.clone();
    let mut out = Vec::new();
    for mv in moves {
        out.push(san_of(&position, *mv));
        position.play_unchecked(*mv);
    }
    out.join(" ")
}

/// Replays one recording (a folder with `session.json`).
pub fn replay_folder(folder: &Path, overrides: &TrackerOverrides, verbose: bool) -> Result<Report> {
    let session_path = folder.join("session.json");
    let meta: SessionMeta =
        serde_json::from_slice(&std::fs::read(&session_path).with_context(|| format!("cannot read {}", session_path.display()))?)
            .with_context(|| format!("{} is not a recording of NicoChess", session_path.display()))?;
    if meta.format > FORMAT {
        bail!("this recording has format {}, newer than this program understands ({FORMAT}): use a newer NicoChess", meta.format);
    }
    let events = group_pictures(read_events(folder)?);

    // Every position the game was in, according to the recording.
    let mut real: HashSet<String> = HashSet::new();
    real.insert(key_of(&meta.start_fen));
    for event in &events {
        if let Some(fen) = event.data["fen"].as_str() {
            real.insert(key_of(fen));
        }
    }

    let config = overrides.apply(meta.tracker.clone());
    let mut tracker = Tracker::new(config.clone());
    tracker.set_view_calibration(0, meta.calibration.as_ref());
    tracker.set_view_calibration(1, meta.calibration2.as_ref());
    tracker.start(parse_fen(&meta.start_fen).context("the start position of the recording")?);

    let mut text = String::new();
    let _ = writeln!(text, "Replay of {}", folder.display());
    let _ = writeln!(
        text,
        "  recorded by NicoChess {} from {}, game {}, started {}",
        meta.program,
        meta.source,
        meta.game_id,
        meta.started_local.as_deref().unwrap_or(&meta.started_utc)
    );
    let _ = writeln!(
        text,
        "  tracker: stable_frames={} stable_time={} motion_span={} motion_threshold={} max_excess={} margin={} soft_margin={} max_plies={}{}",
        config.stable_frames,
        config.stable_time,
        config.motion_span,
        config.motion_threshold,
        config.max_excess,
        config.margin,
        config.soft_margin,
        config.max_plies,
        if overrides.describe().is_empty() { " (as recorded)".to_string() } else { format!("  (changed: {})", overrides.describe()) }
    );
    let _ = writeln!(text);

    let mut summary = Summary::default();
    let base = Instant::now();
    let mut last_file = String::new();
    let mut last_status = String::new();
    for event in &events {
        let now = base + Duration::from_secs_f64(event.t.max(0.0));
        match event.kind.as_str() {
            "views" => {
                // Which cameras gave pictures from now on (see the engine).
                if let Some(active) = event.data["active"].as_array() {
                    for (camera, on) in active.iter().enumerate() {
                        tracker.set_view_active(camera, on.as_bool().unwrap_or(true));
                    }
                }
            }
            "frame" => {
                // The pictures that were given to the tracker together: one per camera.
                let files: Vec<(usize, String)> = event.data["files"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|f| Some((f["cam"].as_u64()? as usize, f["file"].as_str()?.to_string()))).collect())
                    .unwrap_or_default();
                // The two cameras, and the height picture of a depth camera after them.
                let mut images: Vec<Option<image::RgbImage>> = vec![None; 3];
                for (camera, file) in &files {
                    let image = image::open(folder.join(file)).with_context(|| format!("cannot read the picture {file}"))?.to_rgb8();
                    if let Some(place) = images.get_mut(*camera) {
                        *place = Some(image);
                    }
                }
                summary.pictures += files.len();
                last_file = files.iter().map(|(_, f)| f.as_str()).collect::<Vec<_>>().join(" ");
                let before = tracker.position().clone();
                let given: Vec<Option<&image::RgbImage>> = images.iter().map(Option::as_ref).collect();
                let moves = match tracker.feed_views(&given, now) {
                    Detection::Move { mv, .. } => vec![mv],
                    Detection::Sequence { moves, .. } => moves,
                    Detection::Unsure { candidates } if verbose => {
                        let best = candidates.first().map_or(String::new(), |c| format!("{} ({:.0}% sure)", c.san, 100.0 * c.chance));
                        let why = why_unsure(candidates.get(1).map(|c| f64::from(c.cost)), config.margin);
                        let _ = writeln!(text, "{:>8.1}s  replay unsure ({why}), likeliest: {best}   [{last_file}]", event.t);
                        continue;
                    }
                    _ => Vec::new(),
                };
                let doubtful = tracker.take_doubt().map_or(String::new(), |d| format!("   (doubtful, {:.0}% sure)", 100.0 * d.certainty));
                let status = status_text(tracker.status());
                if verbose && status != last_status {
                    let _ = writeln!(text, "{:>8.1}s  replay status: {status}   [{last_file}]", event.t);
                }
                last_status = status.to_string();
                if moves.is_empty() {
                    continue;
                }
                let after = fen_of(tracker.position());
                let ok = real.contains(&key_of(&after));
                if ok {
                    summary.found += 1;
                } else {
                    summary.wrong += 1;
                }
                let _ = writeln!(
                    text,
                    "{:>8.1}s  replay found {}{}{}   [{last_file}]",
                    event.t,
                    sans_of(&before, &moves),
                    doubtful,
                    if ok { "" } else { "   WRONG: a position the game never had" }
                );
            }
            "detect" => {
                summary.live_found += 1;
                let moves = event.data["san"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                if checkpoint(&mut tracker, event.text("fen"))? {
                    summary.behind += 1;
                    let _ =
                        writeln!(text, "{:>8.1}s  live found {moves}; the replay had not: put on the position   [{last_file}]", event.t);
                } else if verbose {
                    let _ = writeln!(text, "{:>8.1}s  live found {moves}; the replay agrees", event.t);
                }
            }
            "manual" => {
                summary.live_typed += 1;
                let typed = event.text("text");
                if checkpoint(&mut tracker, event.text("fen"))? {
                    summary.behind += 1;
                    let _ = writeln!(
                        text,
                        "{:>8.1}s  typed by hand during the game: {typed}; the replay had not found it either   [{last_file}]",
                        event.t
                    );
                } else {
                    summary.typed_found += 1;
                    let _ = writeln!(
                        text,
                        "{:>8.1}s  typed by hand during the game: {typed}; the replay found it by itself   [{last_file}]",
                        event.t
                    );
                }
            }
            "undo" | "promotion" | "doubt" => {
                if event.kind == "undo" {
                    summary.live_undone += 1;
                }
                let _ = writeln!(text, "{:>8.1}s  {} during the game", event.t, event.kind);
                // These change the position outside the tracker, whatever it believes.
                let position = parse_fen(event.text("fen")).context("a position of the recording")?;
                if event.kind == "undo" {
                    // As live: the board still shows the move until the pieces are put back or the move is entered.
                    tracker.undo_to(position, None);
                } else {
                    tracker.set_position(position);
                }
            }
            "relearn" => {
                let _ = writeln!(text, "{:>8.1}s  the squares were learned again during the game", event.t);
                tracker.relearn();
            }
            "calibration" => {
                if let Ok(calibration) = serde_json::from_value::<crate::calibration::Calibration>(event.data["calibration"].clone()) {
                    let camera = event.data["cam"].as_u64().unwrap_or(0) as usize;
                    // The third view is the height picture of a depth camera: it only helps.
                    if camera == 2 {
                        tracker.set_view_advisory(2, true);
                    }
                    // As the engine does: a board that moved a little keeps what was learned.
                    if event.text("how") == "follow" && tracker.status() != Status::Learning {
                        tracker.move_view_calibration(camera, &calibration);
                    } else {
                        tracker.set_view_calibration(camera, Some(&calibration));
                    }
                    let which = if camera == 0 { String::new() } else { format!(" of camera {}", camera + 1) };
                    let _ = writeln!(text, "{:>8.1}s  calibration {}{which} during the game", event.t, event.text("how"));
                }
            }
            "unsure" if verbose => {
                let why = why_unsure(event.data["candidates"][1]["cost"].as_f64(), config.margin);
                let _ = writeln!(text, "{:>8.1}s  live was unsure ({why}): {}", event.t, event.data["candidates"]);
            }
            "end" => {
                let _ = writeln!(text, "{:>8.1}s  game over: {} {}", event.t, event.text("result"), event.text("reason"));
            }
            "closed" => {
                let dropped = event.data["dropped"].as_u64().unwrap_or(0);
                if dropped > 0 {
                    let _ = writeln!(
                        text,
                        "{:>8.1}s  note: {dropped} pictures or lines were dropped while recording (the disk was too slow)",
                        event.t
                    );
                }
            }
            _ => {}
        }
    }

    let _ = writeln!(text);
    let _ = writeln!(text, "{} pictures replayed; the replay ends in {}.", summary.pictures, fen_of(tracker.position()));
    let _ = writeln!(
        text,
        "During the game: {} detections of moves by the program, {} moves typed by hand, {} undone.",
        summary.live_found, summary.live_typed, summary.live_undone
    );
    let _ = writeln!(
        text,
        "Replay: {} detections that fit the game, {} that do not; behind the game at {} of {} places (that many moves would have been typed); {} of the {} moves typed by hand were found by itself.",
        summary.found,
        summary.wrong,
        summary.behind,
        summary.live_found + summary.live_typed,
        summary.typed_found,
        summary.live_typed
    );
    Ok(Report { text, summary })
}

/// Why a tracker was unsure, from the cost of the second candidate (how far it is behind the first): two moves that
/// look alike (which the soft margin takes on), or a picture that no move explains well enough (which it does not).
fn why_unsure(second_cost: Option<f64>, margin: f32) -> &'static str {
    match second_cost {
        Some(cost) if cost < f64::from(margin) => "two moves look alike",
        _ => "the picture is not explained well enough",
    }
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::NoCalibration => "no calibration",
        Status::Learning => "learning",
        Status::Watching => "watching",
        Status::Moving => "moving",
        Status::Unsure => "unsure",
        Status::Undone => "undone",
    }
}

/// The game was in `fen` at this moment. If the tracker believes something else it is put there, as a person
/// typing the move would. True when it had to be put.
fn checkpoint(tracker: &mut Tracker, fen: &str) -> Result<bool> {
    let position = parse_fen(fen).context("a position of the recording")?;
    if key_of(&fen_of(tracker.position())) == key_of(fen) {
        return Ok(false);
    }
    tracker.set_position(position);
    Ok(true)
}

/// Replays a recording folder, or every recording in a folder that holds some. Prints the reports.
pub fn run(path: &Path, overrides: &TrackerOverrides, verbose: bool) -> Result<()> {
    let folders = recordings_in(path)?;
    let mut total = Summary::default();
    for folder in &folders {
        let report = replay_folder(folder, overrides, verbose)?;
        print!("{}", report.text);
        println!();
        total.pictures += report.summary.pictures;
        total.live_found += report.summary.live_found;
        total.live_typed += report.summary.live_typed;
        total.found += report.summary.found;
        total.wrong += report.summary.wrong;
        total.behind += report.summary.behind;
    }
    if folders.len() > 1 {
        println!(
            "All {} recordings: {} pictures; {} detections that fit, {} that do not; behind the game at {} of {} places.",
            folders.len(),
            total.pictures,
            total.found,
            total.wrong,
            total.behind,
            total.live_found + total.live_typed
        );
    }
    Ok(())
}

/// `path` itself when it is a recording, else the recordings among its sub-folders, oldest first.
fn recordings_in(path: &Path) -> Result<Vec<PathBuf>> {
    if path.join("session.json").is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.join("session.json").is_file())
        .collect();
    found.sort();
    if found.is_empty() {
        bail!("{} holds no recording (a folder with session.json, made by --record)", path.display());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::fen_of;
    use crate::picture::Picture;
    use crate::record::{RecordOptions, Recorder, SessionMeta};
    use crate::synthetic::{self, PieceGrid};
    use image::RgbImage;
    use serde_json::json;
    use shakmaty::{Chess, Position, san::San};
    use std::sync::Arc;

    fn fens_after(sans: &[&str]) -> Vec<String> {
        let mut position = Chess::default();
        let mut out = vec![fen_of(&position)];
        for text in sans {
            let mv = text.parse::<San>().unwrap().to_move(&position).unwrap();
            position.play_unchecked(mv);
            out.push(fen_of(&position));
        }
        out
    }

    /// Writes a recording of a game by hand: what the camera showed, and what the people did.
    struct Session {
        recorder: Recorder,
        begun: Instant,
        n: u32,
    }

    impl Session {
        fn new(dir: &Path) -> Session {
            let meta = SessionMeta {
                format: FORMAT,
                program: "test".into(),
                started_utc: String::new(),
                started_local: None,
                source: "synthetic".into(),
                game_id: "test".into(),
                start_fen: fen_of(&Chess::default()),
                resumed_at: 0.0,
                record_every: 0.0,
                tracker: TrackerConfig::default(),
                calibration: Some(synthetic::corners()),
                picture: Picture::default(),
                calibration2: None,
                picture2: None,
            };
            let begun = Instant::now();
            // A test camera is faster than any disk: nothing may be dropped.
            let options = RecordOptions { backlog: 100_000, ..RecordOptions::new(dir.to_path_buf(), Duration::ZERO) };
            Session { recorder: Recorder::begin(&options, meta, begun).unwrap(), begun, n: 0 }
        }

        fn at(&self) -> Instant {
            self.begun + Duration::from_millis(333 * u64::from(self.n))
        }

        fn show(&mut self, grid: &PieceGrid, times: usize) {
            let image: Arc<RgbImage> = Arc::new(synthetic::frame_from_grid(grid));
            for _ in 0..times {
                self.n += 1;
                self.recorder.frame(self.at(), &image, "watching", true);
                self.recorder.wait_idle();
            }
        }

        fn note(&self, kind: &str, data: Value) {
            self.recorder.event(self.at(), kind, data);
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nicochess-replay-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn a_replay_tells_what_it_found_what_was_typed_and_what_it_could_not_find() {
        let dir = scratch("tells");
        let steps = synthetic::game_steps(&["e4", "e5", "Nf3"]);
        let fens = fens_after(&["e4", "e5", "Nf3"]);

        let mut session = Session::new(&dir);
        session.show(&synthetic::start_grid(), 7);
        // e4: the program found it.
        session.show(&steps[0].after, 4);
        session.note("detect", json!({"moves": ["e2e4"], "san": ["e4"], "fen": fens[1]}));
        // e5: the program did not find it during the game (a hand over the board, say): typed. The picture
        // does show it, so a replay can find it.
        session.show(&steps[1].after, 4);
        session.note("manual", json!({"text": "e5", "fen": fens[2]}));
        // Nf3: typed too, and the picture does not show it at all (nothing moved): a replay cannot find it.
        session.show(&steps[1].after, 4);
        session.note("manual", json!({"text": "Nf3", "fen": fens[3]}));
        session.note("end", json!({"result": "*", "reason": "abandoned", "fen": fens[3]}));
        let folder = session.recorder.folder().to_path_buf();
        session.recorder.close();

        let report = replay_folder(&folder, &TrackerOverrides::default(), false).unwrap();
        let summary = &report.summary;
        assert_eq!(summary.pictures, 7 + 4 + 4 + 4);
        assert_eq!((summary.live_found, summary.live_typed, summary.live_undone), (1, 2, 0));
        assert_eq!(summary.wrong, 0, "{}", report.text);
        assert_eq!(summary.found, 2, "e4 and e5: {}", report.text);
        assert_eq!(summary.typed_found, 1, "e5 was typed but the replay found it: {}", report.text);
        assert_eq!(summary.behind, 1, "Nf3 cannot be found in a picture where nothing moved: {}", report.text);
        assert!(report.text.contains("replay found e4"), "{}", report.text);
        assert!(report.text.contains("typed by hand during the game: e5; the replay found it by itself"), "{}", report.text);
        assert!(report.text.contains("typed by hand during the game: Nf3; the replay had not found it either"), "{}", report.text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_move_that_leads_to_a_position_the_game_never_had_is_reported_as_wrong() {
        let dir = scratch("wrong");
        let steps = synthetic::game_steps(&["e4", "e5"]);
        let mut session = Session::new(&dir);
        session.show(&synthetic::start_grid(), 7);
        // The pictures show e4 and then e5, but the game says that d4 was played: e4 was never on the board.
        session.show(&steps[0].after, 4);
        session.note("manual", json!({"text": "d4", "fen": fens_after(&["d4"])[1]}));
        session.show(&steps[1].after, 4);
        let folder = session.recorder.folder().to_path_buf();
        session.recorder.close();
        let report = replay_folder(&folder, &TrackerOverrides::default(), false).unwrap();
        assert!(report.summary.wrong >= 1, "{}", report.text);
        assert!(report.text.contains("WRONG"), "{}", report.text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_folder_of_recordings_is_replayed_in_order_and_a_folder_without_any_is_refused() {
        let dir = scratch("many");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(recordings_in(&dir).is_err(), "an empty folder holds no recording");
        let mut first = Session::new(&dir);
        first.show(&synthetic::start_grid(), 7);
        let a = first.recorder.folder().to_path_buf();
        first.recorder.close();
        let mut second = Session::new(&dir);
        second.show(&synthetic::start_grid(), 7);
        let b = second.recorder.folder().to_path_buf();
        second.recorder.close();
        let mut both = vec![a.clone(), b];
        both.sort();
        assert_eq!(recordings_in(&dir).unwrap(), both);
        assert_eq!(recordings_in(&a).unwrap(), vec![a]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_recording_cut_short_by_a_power_loss_can_still_be_read() {
        let dir = scratch("cut");
        let mut session = Session::new(&dir);
        session.show(&synthetic::start_grid(), 7);
        let folder = session.recorder.folder().to_path_buf();
        session.recorder.close();
        // The last line is half written.
        let mut text = std::fs::read_to_string(folder.join("events.jsonl")).unwrap();
        text.push_str("{\"t\": 9.9, \"type\": \"fr");
        std::fs::write(folder.join("events.jsonl"), text).unwrap();
        let report = replay_folder(&folder, &TrackerOverrides::default(), false).unwrap();
        assert_eq!(report.summary.pictures, 7);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tuning_values_given_on_the_command_line_replace_the_recorded_ones() {
        let recorded = TrackerConfig { stable_frames: 5, ..TrackerConfig::default() };
        let changed = TrackerOverrides { max_excess: Some(99.0), ..TrackerOverrides::default() }.apply(recorded);
        assert_eq!((changed.stable_frames, changed.max_excess, changed.margin), (5, 99.0, 6.0));
        assert!(TrackerOverrides { margin: Some(9.0), ..TrackerOverrides::default() }.describe().contains("margin=9"));
    }
}
