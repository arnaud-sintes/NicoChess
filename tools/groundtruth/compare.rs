//! The game as the program decided it, set beside the game as read from the pictures (see README.md here).
//!
//! `compare <recording folder> <truth.json> [--saved <game.json>] [--replay] [--late 3] [--all]`
//!
//! `truth.json` is `{"recording": "...", "start_fen": "...", "moves": [{"ply": 1, "uci": "e2e4", "san": "e4",
//! "kf": "kf001", "time": 3.99, "confidence": "high"}, ...]}`: the moves of the real game with the keyframe that shows
//! each one and its time in the recording.
//!
//! The decisions come from `events.jsonl` (the `detect` lines of the program, the moves typed by hand, undo and
//! promotion), or with `--replay` from a fresh run of the tracker over the pictures (`replay::replay_folder`, with the
//! tuning of the recording). Positions are compared, not move lists, because a detection may carry several moves, a
//! person may type one, and an undo takes one back. For each move of the truth:
//!
//! - `matched`: the program found the position that move leads to (and in time);
//! - `late`: the same, but more than `--late` seconds (default 3) after the keyframe that shows it;
//! - `typed`: the live game reached it only because a person typed the move;
//! - `wrong`: the live game was in another position at that move number, and never in the right one;
//! - `missed`: the live game never got to that move number, or only by a jump that skipped it.
//!
//! The unsure periods (from the status of the recorded pictures, so those of the live game even with `--replay`) that
//! overlap the time between the previous keyframe and the decision are listed with each move. A report is written next to
//! the truth: `<truth>.compare.json` (`.compare-replay.json` with `--replay`).

use anyhow::{Context, Result, bail};
use nicochess::game::{fen_of, parse_fen, san_of, uci_of};
use nicochess::replay::{TrackerOverrides, replay_folder};
use serde_json::{Value, json};
use shakmaty::uci::UciMove;
use shakmaty::{Chess, Color, Position};
use std::path::{Path, PathBuf};

/// The position without the move counters: what makes two positions the same one.
fn key(fen: &str) -> String {
    fen.split(' ').take(4).collect::<Vec<_>>().join(" ")
}

/// The number of half-moves since the first position of the game was set up (as written in its FEN).
fn ply_of(pos: &Chess) -> i64 {
    2 * (i64::from(pos.fullmoves().get()) - 1) + i64::from(pos.turn() == Color::Black)
}

/// A JSON file written by hand or by a script: a byte order mark at the start (which PowerShell likes to add) is skipped.
fn read_json(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    serde_json::from_slice(bytes).with_context(|| format!("{} is not JSON", path.display()))
}

/// What the program (or a person) did to the game: one event of the recording or one detection of a replay.
struct Live {
    t: f64,
    kind: String,
    before: Option<String>,
    uci: Vec<String>,
    after: String,
}

/// One position the live game was in, with the move that led there when it is known.
struct Step {
    t: f64,
    kind: String,
    ply: i64,
    key: String,
    san: Option<String>,
}

struct Recorded {
    start_fen: String,
    events: Vec<Live>,
    /// Stretches of time (start, end) in which the pictures said the tracker was unsure.
    unsure: Vec<(f64, f64)>,
    relearn: Vec<f64>,
}

fn read_recorded(folder: &Path) -> Result<Recorded> {
    let session: Value = serde_json::from_slice(&std::fs::read(folder.join("session.json")).context("cannot read session.json")?)?;
    let start_fen = session["start_fen"].as_str().context("session.json has no start_fen")?.to_string();
    let text = std::fs::read_to_string(folder.join("events.jsonl")).context("cannot read events.jsonl")?;
    let mut current = start_fen.clone();
    let (mut events, mut unsure, mut relearn) = (Vec::new(), Vec::new(), Vec::new());
    let mut open: Option<(f64, f64)> = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        // A last line cut short by a stop is skipped, as the replay does.
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let (Some(t), Some(kind)) = (e["t"].as_f64(), e["type"].as_str()) else { continue };
        match kind {
            "frame" => {
                if e["status"].as_str() == Some("unsure") {
                    open = Some((open.map_or(t, |o| o.0), t));
                } else if let Some((from, last)) = open.take() {
                    unsure.push((from, last.max(t)));
                }
            }
            "detect" | "manual" | "undo" | "promotion" => {
                let Some(after) = e["fen"].as_str() else { continue };
                let uci =
                    e["moves"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect()).unwrap_or_default();
                events.push(Live { t, kind: kind.to_string(), before: Some(current.clone()), uci, after: after.to_string() });
                current = after.to_string();
            }
            "relearn" => relearn.push(t),
            _ => {}
        }
    }
    if let Some((from, last)) = open {
        unsure.push((from, last));
    }
    Ok(Recorded { start_fen, events, unsure, relearn })
}

/// The positions the live game went through because of one event. A detection of several moves gives each of them, so
/// that a move that was part of a sequence counts as found; if the moves cannot be played from the position before, the
/// position after is the only one.
fn steps_of(event: &Live, start_ply: i64) -> Vec<Step> {
    let Ok(after) = parse_fen(&event.after) else { return Vec::new() };
    if event.kind == "undo" {
        return Vec::new();
    }
    let single = || vec![Step { t: event.t, kind: event.kind.clone(), ply: ply_of(&after) - start_ply, key: key(&event.after), san: None }];
    let Some(Ok(mut pos)) = event.before.as_ref().map(|b| parse_fen(b)) else { return single() };
    if event.kind == "promotion" {
        return single();
    }
    let mut ucis = event.uci.clone();
    if ucis.is_empty() {
        // A move typed by hand: the legal move that leads to the position after.
        let wanted = key(&event.after);
        let found = pos.legal_moves().into_iter().find(|mv| {
            let mut next = pos.clone();
            next.play_unchecked(*mv);
            key(&fen_of(&next)) == wanted
        });
        let Some(mv) = found else { return single() };
        ucis.push(uci_of(mv));
    }
    let mut out = Vec::new();
    for text in &ucis {
        let Some(mv) = text.parse::<UciMove>().ok().and_then(|u| u.to_move(&pos).ok()) else { return single() };
        let san = san_of(&pos, mv);
        pos.play_unchecked(mv);
        out.push(Step { t: event.t, kind: event.kind.clone(), ply: ply_of(&pos) - start_ply, key: key(&fen_of(&pos)), san: Some(san) });
    }
    if out.last().is_some_and(|s| s.key == key(&event.after)) { out } else { single() }
}

/// Seconds of the stretches that overlap [from, to], and how many stretches.
fn overlap(stretches: &[(f64, f64)], from: f64, to: f64) -> (f64, usize) {
    let mut seconds = 0.0;
    let mut count = 0;
    for &(a, b) in stretches {
        if b >= from && a <= to {
            seconds += (b.min(to) - a.max(from)).max(0.0);
            count += 1;
        }
    }
    (seconds, count)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: compare <recording folder> <truth.json> [--saved <game.json>] [--replay] [--late 3] [--all]";
    let (Some(folder), Some(truth_path)) = (args.next(), args.next()) else { bail!(usage) };
    let (folder, truth_path) = (PathBuf::from(folder), PathBuf::from(truth_path));
    let (mut saved, mut replay, mut late, mut all) = (None::<PathBuf>, false, 3.0f64, false);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--saved" => saved = Some(PathBuf::from(args.next().context("--saved needs a file")?)),
            "--replay" => replay = true,
            "--late" => late = args.next().context("--late needs seconds")?.parse()?,
            "--all" => all = true,
            other => bail!("unknown option {other}\n{usage}"),
        }
    }

    // The truth, played from its start position so that every move is checked.
    let truth = read_json(&truth_path)?;
    let mut recorded = read_recorded(&folder)?;
    // Without a start position the truth starts where the recording did.
    let truth_start = truth["start_fen"].as_str().map_or_else(|| recorded.start_fen.clone(), str::to_string);
    if key(&truth_start) != key(&recorded.start_fen) {
        bail!("the truth starts from {truth_start} but the recording from {}", recorded.start_fen);
    }
    let start = parse_fen(&truth_start)?;
    let start_ply = ply_of(&start);
    let mut pos = start.clone();
    struct Truth {
        uci: String,
        san: String,
        kf: String,
        time: f64,
        confidence: String,
        key: String,
    }
    let mut moves: Vec<Truth> = Vec::new();
    for (i, m) in truth["moves"].as_array().context("the truth has no moves")?.iter().enumerate() {
        let uci = m["uci"].as_str().with_context(|| format!("move {} has no uci", i + 1))?;
        let mv = uci
            .parse::<UciMove>()
            .ok()
            .and_then(|u| u.to_move(&pos).ok())
            .with_context(|| format!("move {} of the truth, {uci}, is not legal in {}", i + 1, fen_of(&pos)))?;
        let san = san_of(&pos, mv);
        pos.play_unchecked(mv);
        moves.push(Truth {
            uci: uci.to_string(),
            san,
            kf: m["kf"].as_str().unwrap_or("").to_string(),
            time: m["time"].as_f64().unwrap_or(0.0),
            confidence: m["confidence"].as_str().unwrap_or("").to_string(),
            key: key(&fen_of(&pos)),
        });
    }
    let n = moves.len();

    // The live game: from the recording, or from a fresh run of the tracker.
    let mut source = "events.jsonl of the recording";
    if replay {
        let report = replay_folder(&folder, &TrackerOverrides::default(), false)?;
        recorded.events = report
            .detections
            .into_iter()
            .map(|d| Live { t: d.t, kind: "detect".into(), before: Some(d.before), uci: d.uci, after: d.after })
            .collect();
        source = "a fresh replay of the pictures (positions the replay had to be put on are not counted as found)";
    }
    let steps: Vec<Step> = recorded.events.iter().flat_map(|e| steps_of(e, start_ply)).collect();

    // Per move of the truth.
    let mut rows: Vec<Value> = Vec::new();
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    let mut first_divergence: Option<usize> = None;
    for (i, m) in moves.iter().enumerate() {
        let ply = i as i64 + 1;
        let reached = steps.iter().find(|s| s.ply == ply && s.key == m.key);
        let wrong: Vec<&Step> = steps.iter().filter(|s| s.ply == ply && s.key != m.key).collect();
        let category = match reached {
            Some(s) if s.kind == "manual" => "typed",
            Some(s) if s.t - m.time > late => "late",
            Some(_) => "matched",
            None if !wrong.is_empty() => "wrong",
            None => "missed",
        };
        *counts.entry(category).or_default() += 1;
        if first_divergence.is_none() && !matches!(category, "matched" | "late") {
            first_divergence = Some(i);
        }
        // Between the previous keyframe and the decision (or, when there was none, a little after the keyframe).
        let from = moves.iter().map(|o| o.time).filter(|&t| t < m.time).fold(0.0, f64::max);
        let to = reached.map_or(m.time + late, |s| s.t.max(m.time));
        let (unsure_s, unsure_n) = overlap(&recorded.unsure, from, to);
        rows.push(json!({
            "ply": ply, "truth_san": m.san, "truth_uci": m.uci, "kf": m.kf, "time": m.time, "confidence": m.confidence,
            "category": category,
            "live_time": reached.map(|s| s.t),
            "live_kind": reached.map(|s| s.kind.clone()),
            "seconds_after_keyframe": reached.map(|s| s.t - m.time),
            "wrong_before": wrong.iter().map(|s| json!({"t": s.t, "san": s.san, "kind": s.kind})).collect::<Vec<_>>(),
            "unsure_seconds": unsure_s, "unsure_stretches": unsure_n,
        }));
    }
    let final_live = recorded.events.iter().rev().find(|e| e.kind != "relearn").map(|e| key(&e.after));
    let extra = steps.iter().map(|s| s.ply).max().map_or(0, |p| (p - n as i64).max(0));
    let final_ok = moves.last().map(|m| m.key.clone()).or_else(|| Some(key(&truth_start))) == final_live;
    let get = |c: &str| counts.get(c).copied().unwrap_or(0);
    let by_itself = get("matched") + get("late");

    // The saved game, when given: the final list of moves after every correction.
    let saved_line = match &saved {
        Some(path) => {
            let game = read_json(path)?;
            let list: Vec<String> = game["moves"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| m["uci"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let same = list.iter().zip(&moves).take_while(|(a, b)| **a == b.uci).count();
            Some(if same == n && list.len() == n {
                format!("saved game: the same {n} moves as the truth")
            } else {
                format!("saved game: {} moves, the same as the truth for the first {same} (truth {n})", list.len())
            })
        }
        None => None,
    };

    println!("Truth {} ({n} moves) against {source}", truth_path.display());
    println!(
        "found by the program {by_itself}/{n} (matched {}, late {}); typed by hand {}; wrong {}; missed {}; extra live moves {extra}; live game ends in the truth's position: {}",
        get("matched"),
        get("late"),
        get("typed"),
        get("wrong"),
        get("missed"),
        if final_ok { "yes" } else { "no" }
    );
    if let Some(line) = &saved_line {
        println!("{line}");
    }
    if !recorded.relearn.is_empty() {
        println!("squares learned again at {:?} s", recorded.relearn);
    }
    match first_divergence {
        Some(i) => println!(
            "first divergence: move {} {} (keyframe {}, {:.1} s): {}",
            i + 1,
            moves[i].san,
            moves[i].kf,
            moves[i].time,
            rows[i]["category"].as_str().unwrap_or("")
        ),
        None => println!("no divergence: every move was found by the program"),
    }
    println!("\n ply  move      kf      time   category  decided   unsure  notes");
    for row in &rows {
        let cat = row["category"].as_str().unwrap_or("");
        if !all && matches!(cat, "matched") {
            continue;
        }
        let live = row["live_time"].as_f64().map_or("-".to_string(), |t| format!("{t:.1}s"));
        let mut notes = Vec::new();
        for w in row["wrong_before"].as_array().into_iter().flatten() {
            notes.push(format!("live had {} at {:.1}s", w["san"].as_str().unwrap_or("?"), w["t"].as_f64().unwrap_or(0.0)));
        }
        println!(
            "{:>4}  {:<8}  {:<6} {:>6.1}  {:<8}  {:>7}  {:>5.1}s  {}",
            row["ply"].as_i64().unwrap_or(0),
            row["truth_san"].as_str().unwrap_or(""),
            row["kf"].as_str().unwrap_or(""),
            row["time"].as_f64().unwrap_or(0.0),
            cat,
            live,
            row["unsure_seconds"].as_f64().unwrap_or(0.0),
            notes.join("; ")
        );
    }

    let out = truth_path.with_extension(if replay { "compare-replay.json" } else { "compare.json" });
    let report = json!({
        "recording": folder.display().to_string(), "truth": truth_path.display().to_string(), "source": source,
        "late_seconds": late, "plies": n,
        "counts": {"matched": get("matched"), "late": get("late"), "typed": get("typed"), "wrong": get("wrong"), "missed": get("missed")},
        "extra_live_plies": extra, "live_ends_in_truth_position": final_ok,
        "first_divergence_ply": first_divergence.map(|i| i + 1),
        "saved": saved_line, "moves": rows,
    });
    nicochess::fsutil::write_atomic(&out, serde_json::to_string_pretty(&report)?.as_bytes())?;
    println!("\nreport written to {}", out.display());
    Ok(())
}
