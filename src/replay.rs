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

use crate::analysis::{Analyzer, Context as LookContext, Item};
use crate::game::{fen_of, parse_fen, san_of, uci_of};
use crate::record::{FORMAT, SessionMeta};
use crate::tracker::{Detection, Status, Tracker, TrackerConfig};
use crate::video::{PictureRef, Pictures};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use shakmaty::Move;
use std::collections::{HashMap, HashSet};
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
    /// What the replayed tracker found by itself, in order, for tools that set it beside another reading of the game
    /// (`tools/groundtruth`). A place where the replay had to be put on the real position is not in the list.
    pub detections: Vec<Found>,
}

/// One detection of the replayed tracker: the moves (one or a sequence) with the time of the picture that gave them.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub t: f64,
    pub before: String,
    pub uci: Vec<String>,
    pub san: Vec<String>,
    pub after: String,
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
        let file = serde_json::json!({
            "cam": event.data["cam"].as_u64().unwrap_or(0),
            "file": event.text("file"),
            "video": event.text("video"),
            "index": event.data["index"].as_u64().unwrap_or(0),
        });
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

/// What a replay feeds the tracker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Feed {
    /// As it was: the pictures that the tracker was given together, at the times it was given them (the default).
    #[default]
    AsLive,
    /// Every picture that was recorded, one after the other in the order they were captured, each at the moment it was
    /// captured (`--replay-all`; a recording that kept only some of the pictures, made with `--record`, replays as it was).
    EveryPicture,
}

/// Replays one recording (a folder with `session.json`), as the tracker saw it live.
pub fn replay_folder(folder: &Path, overrides: &TrackerOverrides, verbose: bool) -> Result<Report> {
    replay_folder_with(folder, overrides, verbose, Feed::AsLive)
}

/// Replays one recording, feeding the tracker as `feed` says.
pub fn replay_folder_with(folder: &Path, overrides: &TrackerOverrides, verbose: bool, feed: Feed) -> Result<Report> {
    let session_path = folder.join("session.json");
    let meta: SessionMeta =
        serde_json::from_slice(&std::fs::read(&session_path).with_context(|| format!("cannot read {}", session_path.display()))?)
            .with_context(|| format!("{} is not a recording of NicoChess", session_path.display()))?;
    if meta.format > FORMAT {
        bail!("this recording has format {}, newer than this program understands ({FORMAT}): use a newer NicoChess", meta.format);
    }
    // The videos of a recording that keeps its pictures in video files are told by lines of their own, before the
    // pictures; they must not break up the pictures of one time.
    let (video_lines, events): (Vec<Event>, Vec<Event>) = read_events(folder)?.into_iter().partition(|e| e.kind == "video");
    let mut pictures = Pictures::new(folder);
    for line in &video_lines {
        pictures.note_video(&line.data);
    }
    // A recording in video needs ffmpeg: say so before anything else.
    pictures.require_ffmpeg_for_videos()?;
    let mut unreadable = 0usize;
    // A recording of every picture: the pictures are told one by one by `frame` lines with a number (`seq`), and the `look`
    // lines name the ones that the tracker was given together. They are found before anything is replayed, since a look can
    // come before the line of a picture that another thread was still handing over (the depth view).
    let every_picture = meta.every_picture;
    let mut kept: HashMap<(usize, u64), PictureRef> = HashMap::new();
    let mut events = events;
    if every_picture {
        let mut rest = Vec::with_capacity(events.len());
        let mut sources = Vec::new();
        for mut event in events {
            if event.kind == "frame"
                && let (Some(cam), Some(seq), Some(picture)) =
                    (event.data["cam"].as_u64(), event.data["seq"].as_u64(), PictureRef::of_event(&event.data))
            {
                kept.insert((cam as usize, seq), picture);
                if feed == Feed::EveryPicture {
                    event.kind = "source".to_string();
                    sources.push(event);
                }
                continue;
            }
            rest.push(event);
        }
        events = if feed == Feed::EveryPicture { interleave(rest, sources) } else { rest };
    }
    let events = group_pictures(events);
    // Brightness and contrast are applied to the pictures of a recording of every picture, as the engine did live.
    let adjusting = [Some(meta.picture), meta.picture2, None];
    let mut missing = 0usize;
    let mut last_fed = 0.0f64;

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
    let mut analyzer = Analyzer::new(tracker);
    analyzer.begin();
    // What the live engine told the analysis at each look (see `analysis::Context`), as far as a recording says it: which
    // cameras have a calibration (and the second is in use), whether the depth camera found the squares, and until when
    // a game was played.
    let second_in_use = meta.picture2.is_some();
    let mut calibrated = [meta.calibration.is_some(), meta.calibration2.is_some() && second_in_use];
    let mut depth_found = false;
    let mut playing = true;
    let interval = Duration::from_secs_f64(meta.track_interval.clamp(0.0, 10.0));
    let fresh = crate::engine::EngineConfig::default().fresh;

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
    if every_picture {
        let _ = writeln!(
            text,
            "  every picture of every camera was recorded ({} of them); replayed as {}",
            kept.len(),
            if feed == Feed::EveryPicture {
                "every picture, one after the other, at the moment it was captured"
            } else {
                "the tracker saw them live: the pictures it was given together, at the times it was given them"
            }
        );
    }
    let _ = writeln!(text);

    let mut summary = Summary::default();
    let mut detections: Vec<Found> = Vec::new();
    let base = Instant::now();
    let mut last_file = String::new();
    let mut last_status = String::new();
    for event in &events {
        let mut now = base + Duration::from_secs_f64(event.t.max(0.0));
        // The time the tracker was given at a look is the capture time of the picture that began it (a recording
        // before format 5 has the time of the look).
        if event.kind == "look"
            && let Some(at) = event.data["at"].as_f64()
        {
            now = base + Duration::from_secs_f64(at.max(0.0));
        }
        match event.kind.as_str() {
            "views" => {
                // Which cameras gave pictures from now on (see the engine).
                if let Some(active) = event.data["active"].as_array() {
                    // With `--replay-all` the analysis decides which cameras take part, as it did live.
                    for (camera, on) in active.iter().enumerate().filter(|_| feed == Feed::AsLive) {
                        analyzer.tracker.set_view_active(camera, on.as_bool().unwrap_or(true));
                    }
                }
            }
            // A look of the tracker, in a recording of every picture: the pictures it was given together. With
            // `--replay-all` the looks are not followed: every picture is fed as it was captured.
            "look" if feed == Feed::EveryPicture => {}
            "frame" | "look" | "source" => {
                // The pictures that were given to the tracker together: one per camera.
                let files: Vec<(usize, PictureRef)> = match event.kind.as_str() {
                    "look" => {
                        let named: Vec<(usize, u64)> = event.data["pictures"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|p| Some((p["cam"].as_u64()? as usize, p["seq"].as_u64()?))).collect())
                            .unwrap_or_default();
                        let mut files = Vec::new();
                        for key in named {
                            match kept.get(&key) {
                                Some(picture) => files.push((key.0, picture.clone())),
                                // Left out of the recording (the budget was full), or the depth picture had no flat surface
                                // to be made from: the tracker was given it live, the replay cannot give it.
                                None => missing += 1,
                            }
                        }
                        files
                    }
                    "source" => {
                        // One picture, at the moment it was captured, never before the one fed last.
                        let captured = event.data["captured"].as_f64().unwrap_or(event.t);
                        last_fed = last_fed.max(captured);
                        now = base + Duration::from_secs_f64(last_fed.max(0.0));
                        match (event.data["cam"].as_u64(), PictureRef::of_event(&event.data)) {
                            (Some(cam), Some(picture)) => vec![(cam as usize, picture)],
                            _ => Vec::new(),
                        }
                    }
                    _ => event.data["files"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|f| Some((f["cam"].as_u64()? as usize, PictureRef::of_event(f)?))).collect())
                        .unwrap_or_default(),
                };
                // The two cameras, and the height picture of a depth camera after them.
                let mut images: Vec<Option<image::RgbImage>> = vec![None; 3];
                for (camera, picture) in &files {
                    let image = match pictures.load(picture) {
                        Ok(image) => image,
                        // A video cut short (the encoder died, or the program was killed): the pictures that are not
                        // in it are missing, as if the camera had given none; a file that cannot be read is an error.
                        Err(e) if picture.file.is_none() => {
                            unreadable += 1;
                            if unreadable == 1 {
                                let _ = writeln!(
                                    text,
                                    "{:>8.1}s  a picture of a video cannot be read ({e:#}); the pictures that cannot are left out",
                                    event.t
                                );
                            }
                            continue;
                        }
                        Err(e) => return Err(e),
                    };
                    if let Some(place) = images.get_mut(*camera) {
                        // The pictures of a recording of every picture are as the camera gave them, turned: the tracker
                        // was given them with brightness and contrast applied, and so it is now.
                        let image = match adjusting.get(*camera).copied().flatten() {
                            Some(settings) if every_picture => settings.adjust(&image).unwrap_or(image),
                            _ => image,
                        };
                        *place = Some(image);
                    }
                }
                summary.pictures += files.len();
                last_file = files.iter().map(|(_, p)| p.name()).collect::<Vec<_>>().join(" ");
                let before = analyzer.tracker.position().clone();
                let detection = if event.kind == "source" {
                    // One picture, at the moment it was captured: the analysis decides by itself whether the tracker looks
                    // (the same rule as the live program, on the time of capture).
                    let Some((camera, _)) = files.first() else { continue };
                    let Some(image) = images.get_mut(*camera).and_then(Option::take) else { continue };
                    let image = std::sync::Arc::new(image);
                    let item = Item {
                        view: *camera,
                        seq: event.data["seq"].as_u64().unwrap_or(0),
                        captured: now,
                        waited: None,
                        image: std::sync::Arc::clone(&image),
                        raw: image,
                    };
                    let context = || LookContext { playing, usable: calibrated, depth: depth_found, interval, fresh, begun_at: Some(base) };
                    match analyzer.on_picture(item, context, interval) {
                        Some(look) => look.detection,
                        None => continue,
                    }
                } else {
                    let given: Vec<Option<&image::RgbImage>> = images.iter().map(Option::as_ref).collect();
                    analyzer.tracker.feed_views(&given, now)
                };
                let moves = match detection {
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
                let doubtful =
                    analyzer.tracker.take_doubt().map_or(String::new(), |d| format!("   (doubtful, {:.0}% sure)", 100.0 * d.certainty));
                let status = status_text(analyzer.tracker.status());
                if verbose && status != last_status {
                    let _ = writeln!(text, "{:>8.1}s  replay status: {status}   [{last_file}]", event.t);
                }
                last_status = status.to_string();
                if moves.is_empty() {
                    continue;
                }
                let after = fen_of(analyzer.tracker.position());
                detections.push(Found {
                    t: event.t,
                    before: fen_of(&before),
                    uci: moves.iter().map(|m| uci_of(*m)).collect(),
                    san: sans_of(&before, &moves).split(' ').map(str::to_string).collect(),
                    after: after.clone(),
                });
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
                if checkpoint(&mut analyzer.tracker, event.text("fen"))? {
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
                if checkpoint(&mut analyzer.tracker, event.text("fen"))? {
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
                    analyzer.tracker.undo_to(position, None);
                } else {
                    analyzer.tracker.set_position(position);
                }
            }
            "relearn" => {
                let _ = writeln!(text, "{:>8.1}s  the squares were learned again during the game", event.t);
                analyzer.tracker.relearn();
            }
            "calibration" => {
                if let Ok(calibration) = serde_json::from_value::<crate::calibration::Calibration>(event.data["calibration"].clone()) {
                    let camera = event.data["cam"].as_u64().unwrap_or(0) as usize;
                    // The third view is the height picture of a depth camera: it only helps.
                    if camera == 2 {
                        analyzer.tracker.set_view_advisory(2, true);
                        depth_found = true;
                    } else if camera < 2 {
                        calibrated[camera] = camera == 0 || second_in_use;
                    }
                    // As the engine does: a board that moved a little keeps what was learned.
                    if event.text("how") == "follow" && analyzer.tracker.status() != Status::Learning {
                        analyzer.tracker.move_view_calibration(camera, &calibration);
                    } else {
                        analyzer.tracker.set_view_calibration(camera, Some(&calibration));
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
                playing = false;
                let _ = writeln!(text, "{:>8.1}s  game over: {} {}", event.t, event.text("result"), event.text("reason"));
            }
            "gap" => {
                let _ = writeln!(
                    text,
                    "{:>8.1}s  the recording is not complete here: {} pictures of camera {} were left out (numbers {} to {}; the disk or the encoder fell behind past the memory limit)",
                    event.t, event.data["count"], event.data["cam"], event.data["first_seq"], event.data["last_seq"]
                );
            }
            "closed" => {
                let dropped = event.data["dropped"].as_u64().unwrap_or(0);
                if dropped > 0 {
                    let _ = writeln!(
                        text,
                        "{:>8.1}s  note: {dropped} pictures or lines were dropped while recording (the disk or the encoder was too slow, or the encoder failed)",
                        event.t
                    );
                }
            }
            _ => {}
        }
    }

    let _ = writeln!(text);
    if unreadable > 0 {
        let _ = writeln!(text, "{unreadable} pictures of the videos could not be read and were left out.");
    }
    if missing > 0 {
        let _ = writeln!(
            text,
            "{missing} pictures that the tracker was given live are not in the recording (left out by a full budget, or a depth picture with no flat surface in it)."
        );
    }
    let _ = writeln!(text, "{} pictures replayed; the replay ends in {}.", summary.pictures, fen_of(analyzer.tracker.position()));
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
    Ok(Report { text, summary, detections })
}

/// A replay of every picture feeds the pictures in the order they were captured, and puts the other lines of the recording
/// between them where the analysis had them: a `look` line after the last picture that the tracker was given at that look
/// (so the detection that follows it is checked against what the replay found), and a line of a command (`after`: how
/// many pictures of each view had come when it was sent) after the pictures that came before it, as the analysis thread
/// does. The pictures that come after the last such line are put at the end.
fn interleave(events: Vec<Event>, mut sources: Vec<Event>) -> Vec<Event> {
    let view = |e: &Event| e.data["cam"].as_u64().unwrap_or(0);
    let seq = |e: &Event| e.data["seq"].as_u64().unwrap_or(0);
    let captured = |e: &Event| e.data["captured"].as_f64().unwrap_or(e.t);
    sources.sort_by(|a, b| captured(a).total_cmp(&captured(b)).then(view(a).cmp(&view(b))).then(seq(a).cmp(&seq(b))));
    let mut slots: Vec<Option<Event>> = sources.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(events.len() + slots.len());
    let mut taken = 0usize;
    for event in events {
        let last: Option<usize> = {
            let from = taken;
            let fits = |source: &Event| -> bool {
                if event.kind == "look" {
                    event.data["pictures"].as_array().is_some_and(|named| {
                        named.iter().any(|p| p["cam"].as_u64() == Some(view(source)) && p["seq"].as_u64() == Some(seq(source)))
                    })
                } else if let Some(after) = event.data["after"].as_array() {
                    after.get(view(source) as usize).and_then(Value::as_u64).is_some_and(|n| seq(source) <= n)
                } else {
                    false
                }
            };
            slots.iter().enumerate().skip(from).rev().find(|(_, s)| s.as_ref().is_some_and(&fits)).map(|(i, _)| i)
        };
        if let Some(last) = last {
            while taken <= last {
                if let Some(source) = slots[taken].take() {
                    out.push(source);
                }
                taken += 1;
            }
        }
        out.push(event);
    }
    // The pictures that no line comes after were taken when nothing more was decided (the game was over): they come last.
    out.extend(slots.into_iter().flatten());
    out
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
pub fn run(path: &Path, overrides: &TrackerOverrides, verbose: bool, feed: Feed) -> Result<()> {
    let folders = recordings_in(path)?;
    let mut total = Summary::default();
    for folder in &folders {
        let report = replay_folder_with(folder, overrides, verbose, feed)?;
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
        /// The number of the last picture given to `source`.
        seq: u64,
    }

    impl Session {
        fn new(dir: &Path) -> Session {
            Session::with_ffmpeg(dir, None)
        }

        /// A recording whose pictures go into video files through this `ffmpeg`, when there is one.
        fn with_ffmpeg(dir: &Path, ffmpeg: Option<PathBuf>) -> Session {
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
                video: None,
                every_picture: false,
                track_interval: 0.0,
            };
            let begun = Instant::now();
            // A test camera is faster than any disk: nothing may be dropped (the budget is far from being reached).
            let options = RecordOptions { ffmpeg, ..RecordOptions::new(dir.to_path_buf(), Duration::ZERO) };
            Session { recorder: Recorder::begin(&options, meta, begun).unwrap(), begun, n: 0, seq: 0 }
        }

        /// A recording of every picture: the pictures are given to `source` as they come, and the tracker looks at
        /// one of every `look_every` of them (`show_captured`).
        fn every_picture(dir: &Path, picture: Picture) -> Session {
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
                picture,
                calibration2: None,
                picture2: None,
                video: None,
                every_picture: true,
                track_interval: 0.0,
            };
            let begun = Instant::now();
            let options = RecordOptions::keeping_all(dir.to_path_buf());
            Session { recorder: Recorder::begin(&options, meta, begun).unwrap(), begun, n: 0, seq: 0 }
        }

        /// `times` pictures of the board as it is, every one kept as captured; the tracker looks at every `look_every`-th.
        fn show_captured(&mut self, grid: &PieceGrid, times: usize, look_every: usize) {
            let image: Arc<RgbImage> = Arc::new(synthetic::frame_from_grid(grid));
            for i in 0..times {
                self.n += 1;
                self.seq += 1;
                let at = self.at();
                self.recorder.source(&crate::stage::Stamped { view: 0, seq: self.seq, captured: at, image: Arc::clone(&image) });
                if i % look_every == 0 {
                    self.recorder.look(at, &[(0, self.seq)], "watching", at);
                }
                self.recorder.wait_idle();
            }
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
    fn a_replay_reads_the_pictures_of_a_recording_kept_as_video() {
        let dir = scratch("video");
        let ffmpeg = crate::video::tests::fake_ffmpeg();
        // What the replay uses to read a video (`--ffmpeg` in the program).
        crate::video::set_explicit(ffmpeg.clone());
        let steps = synthetic::game_steps(&["e4", "e5"]);
        let fens = fens_after(&["e4", "e5"]);
        let mut session = Session::with_ffmpeg(&dir, Some(ffmpeg));
        session.show(&synthetic::start_grid(), 7);
        session.show(&steps[0].after, 4);
        session.note("detect", json!({"moves": ["e2e4"], "san": ["e4"], "fen": fens[1]}));
        session.note("end", json!({"result": "*", "reason": "abandoned", "fen": fens[1]}));
        let folder = session.recorder.folder().to_path_buf();
        session.recorder.close();
        assert!(folder.join("cam0.mkv").is_file(), "the pictures are in a video");
        assert_eq!(std::fs::read_dir(folder.join("frames")).unwrap().count(), 0, "and in no file");

        let report = replay_folder(&folder, &TrackerOverrides::default(), false).unwrap();
        assert_eq!(report.summary.pictures, 11);
        assert_eq!((report.summary.found, report.summary.wrong, report.summary.behind), (1, 0, 0), "{}", report.text);
        assert!(report.text.contains("replay found e4"), "{}", report.text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_recording_of_every_picture_replays_as_the_tracker_saw_it_or_picture_by_picture() {
        let dir = scratch("every");
        let steps = synthetic::game_steps(&["e4", "e5"]);
        let fens = fens_after(&["e4", "e5"]);
        let mut session = Session::every_picture(&dir, Picture::default());
        // 14 pictures of the start position and 8 of e4 are captured; the tracker looks at every second one.
        session.show_captured(&synthetic::start_grid(), 14, 2);
        session.show_captured(&steps[0].after, 8, 2);
        session.note("detect", json!({"moves": ["e2e4"], "san": ["e4"], "fen": fens[1]}));
        // A look that names a picture the recording does not have (it was left out, or never made).
        session.note("look", json!({"feed": 99, "status": "watching", "pictures": [{"cam": 0, "seq": 999}]}));
        session.note("end", json!({"result": "*", "reason": "abandoned", "fen": fens[1]}));
        let folder = session.recorder.folder().to_path_buf();
        session.recorder.close();

        // As it was: the pictures that the looks name.
        let live = replay_folder(&folder, &TrackerOverrides::default(), false).unwrap();
        assert_eq!(live.summary.pictures, 7 + 4, "{}", live.text);
        assert_eq!((live.summary.found, live.summary.wrong, live.summary.behind), (1, 0, 0), "{}", live.text);
        assert!(live.text.contains("1 pictures that the tracker was given live are not in the recording"), "{}", live.text);
        assert!(live.text.contains("every picture of every camera was recorded (22 of them)"), "{}", live.text);
        // Picture by picture: every one that was captured.
        let all = replay_folder_with(&folder, &TrackerOverrides::default(), false, Feed::EveryPicture).unwrap();
        assert_eq!(all.summary.pictures, 22, "{}", all.text);
        assert_eq!((all.summary.found, all.summary.wrong, all.summary.behind), (1, 0, 0), "{}", all.text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_brightness_and_contrast_of_the_session_are_applied_again_to_the_pictures_of_a_recording_of_every_picture() {
        // The pictures are kept as the camera gave them (turned only): the tracker saw them adjusted. With a brightness
        // that whitens everything the board cannot be read, so nothing is found; with none, e4 is.
        let steps = synthetic::game_steps(&["e4"]);
        let fens = fens_after(&["e4"]);
        let mut found = Vec::new();
        for (name, picture) in [("plain", Picture::default()), ("white", Picture { brightness: 255.0, ..Picture::default() })] {
            let dir = scratch(&format!("adjust-{name}"));
            let mut session = Session::every_picture(&dir, picture);
            session.show_captured(&synthetic::start_grid(), 8, 1);
            session.show_captured(&steps[0].after, 6, 1);
            session.note("detect", json!({"moves": ["e2e4"], "san": ["e4"], "fen": fens[1]}));
            let folder = session.recorder.folder().to_path_buf();
            session.recorder.close();
            let report = replay_folder(&folder, &TrackerOverrides::default(), false).unwrap();
            found.push(report.summary.found);
            std::fs::remove_dir_all(&dir).ok();
        }
        assert_eq!(found, [1, 0], "found with the plain picture and with a brightness that whitens everything");
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
