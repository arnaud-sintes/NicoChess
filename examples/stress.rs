//! Stress test of the move tracker on a synthetic camera that gets harder: the light drifts, a shadow
//! moves over the board, a glare spot sits on a square, and pieces of different sizes lean over their
//! neighbours. Whole games are played; when the tracker is not sure (or wrong) a simulated person types
//! the move in, as in real use. It prints how many moves the tracker followed by itself.
//!
//!     cargo run --release --example stress
//!
//! Real recordings are better, and there are none yet: this is what we have to measure the effect of a
//! change to the tracker. A change should not make the numbers worse.

use nicochess::synthetic::{self, Conditions, OPERA_GAME};
use nicochess::tracker::{Detection, Tracker, TrackerConfig};
use shakmaty::fen::Fen;
use shakmaty::san::San;
use shakmaty::{Chess, EnPassantMode, Move, Position};
use std::time::{Duration, Instant};

/// A game as the position before each move and the moves.
struct Game {
    name: String,
    positions: Vec<Chess>,
    moves: Vec<Move>,
}

fn opera() -> Game {
    let mut pos = Chess::default();
    let (mut positions, mut moves) = (vec![pos.clone()], Vec::new());
    for text in OPERA_GAME {
        let mv = text.parse::<San>().unwrap().to_move(&pos).unwrap();
        pos.play_unchecked(mv);
        positions.push(pos.clone());
        moves.push(mv);
    }
    Game { name: "opera game".into(), positions, moves }
}

/// Random legal moves: queens fly over the board, pieces are taken: an unkind test.
fn random_game(seed: u64, plies: usize) -> Game {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut pos = Chess::default();
    let (mut positions, mut moves) = (vec![pos.clone()], Vec::new());
    for _ in 0..plies {
        if pos.outcome().is_known() {
            break;
        }
        let legal = pos.legal_moves();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let mv = legal[(state % legal.len() as u64) as usize];
        pos.play_unchecked(mv);
        positions.push(pos.clone());
        moves.push(mv);
    }
    Game { name: format!("random game {seed}"), positions, moves }
}

fn key(pos: &Chess) -> String {
    Fen::from_position(pos, EnPassantMode::Legal).to_string().split(' ').take(4).collect::<Vec<_>>().join(" ")
}

#[derive(Default, Clone)]
struct Outcome {
    plies: usize,
    auto: usize,
    wrong: usize,
    manual: usize,
    longest_manual_run: usize,
    /// For every ply where the tracker was unsure and offered candidates: how far the first was ahead of the
    /// second (nats), and whether the first was the move that was played.
    unsure: Vec<(f32, bool)>,
    /// For every ply where moves were taken although the tracker was not sure: the certainty it gave, and whether
    /// the game was right after them.
    doubts: Vec<(f32, bool)>,
}

/// The moves a detection brings (none for the other kinds).
fn moves_of(detection: &Detection) -> Vec<Move> {
    match detection {
        Detection::Move { mv, .. } => vec![*mv],
        Detection::Sequence { moves, .. } => moves.clone(),
        _ => Vec::new(),
    }
}

/// `unseen`: every this-many plies, no picture is taken (the tracker must catch up on two moves at once).
fn play(game: &Game, conditions: &Conditions, config: &TrackerConfig, unseen: Option<usize>) -> Outcome {
    let mut tracker = Tracker::new(config.clone());
    tracker.set_calibration(Some(&synthetic::corners()));
    tracker.start(game.positions[0].clone());
    let mut now = Instant::now();
    let mut frames = 0u64;
    // The certainty of the last moves taken in doubt, read after the pictures of a ply were shown.
    let doubt_cell = std::cell::Cell::new(None::<f32>);
    let mut show = |tracker: &mut Tracker, pos: &Chess, progress: f32, times: usize| -> Vec<Detection> {
        let mut out = Vec::new();
        for _ in 0..times {
            frames += 1;
            now += Duration::from_millis(333);
            let detection = tracker.feed(&synthetic::frame_from_position(pos, conditions, progress, frames), now);
            if matches!(detection, Detection::Move { .. } | Detection::Sequence { .. })
                && let Some(doubt) = tracker.take_doubt()
            {
                doubt_cell.set(Some(doubt.certainty));
            }
            if !matches!(detection, Detection::Nothing) {
                out.push(detection);
            }
        }
        out
    };

    show(&mut tracker, &game.positions[0], 0.0, 7);
    let mut known = game.positions[0].clone(); // what the person using the program would see recorded
    let (mut outcome, mut run) = (Outcome::default(), 0usize);
    let total = game.moves.len();
    let mut skipped = 0usize;
    for k in 0..total {
        outcome.plies += 1;
        let truth = &game.positions[k + 1];
        if unseen.is_some_and(|every| k % every == every - 1 && k + 1 < total) {
            skipped += 1; // nobody looks at this move: the next picture shows two
            continue;
        }
        if std::env::var("STRESS_VERBOSE").is_ok() {
            println!("--- ply {} {}", k + 1, nicochess::game::uci_of(game.moves[k]));
        }
        doubt_cell.set(None);
        let detections = show(&mut tracker, truth, (k + 1) as f32 / total as f32, 4);
        let doubt_taken = doubt_cell.take();
        let mut brought_moves = false;
        for detection in &detections {
            for mv in moves_of(detection) {
                if known.is_legal(mv) {
                    known.play_unchecked(mv);
                    brought_moves = true;
                }
            }
        }
        let verbose = std::env::var("STRESS_VERBOSE").is_ok();
        if !brought_moves && skipped == 0 {
            let last_unsure =
                detections.iter().rev().find_map(|d| if let Detection::Unsure { candidates } = d { Some(candidates) } else { None });
            if let Some(candidates) = last_unsure.filter(|c| !c.is_empty()) {
                let gap = candidates.get(1).map_or(99.0, |c| c.cost);
                outcome.unsure.push((gap, candidates[0].uci == nicochess::game::uci_of(game.moves[k])));
            }
        }
        let ok = key(&known) == key(truth) && brought_moves;
        if let Some(certainty) = doubt_taken {
            outcome.doubts.push((certainty, ok));
        }
        if verbose && !ok {
            let seen: Vec<String> = detections.iter().flat_map(moves_of).map(nicochess::game::uci_of).collect();
            println!(
                "   {} ply {:>2} {}: {} status={:?} detected={:?}",
                game.name,
                k + 1,
                nicochess::game::uci_of(game.moves[k]),
                if brought_moves { "WRONG" } else { "typed" },
                tracker.status(),
                seen
            );
        }
        if ok {
            outcome.auto += 1 + skipped; // the unseen moves were found with it
            run = 0;
        } else {
            if brought_moves {
                outcome.wrong += 1;
            } else {
                outcome.manual += 1;
                run += 1;
                outcome.longest_manual_run = outcome.longest_manual_run.max(run);
            }
            outcome.manual += skipped;
            // The person sees the mistake or the doubt and types the move.
            known = truth.clone();
            tracker.set_position(truth.clone());
            // Typing takes a few seconds, during which the camera goes on showing the same board.
            let late = show(&mut tracker, truth, (k + 1) as f32 / total as f32, 3);
            // Nothing may be detected while the picture shows what the person has just typed.
            if late.iter().any(|d| !moves_of(d).is_empty()) {
                outcome.wrong += 1;
                if verbose {
                    let seen: Vec<String> = late.iter().flat_map(moves_of).map(nicochess::game::uci_of).collect();
                    println!("   {} ply {:>2}: WRONG moves detected after the move was typed: {:?}", game.name, k + 1, seen);
                }
            }
        }
        skipped = 0;
    }
    outcome
}

fn main() {
    let games = [opera(), random_game(1, 70), random_game(2, 70), random_game(3, 70)];
    let mut config = TrackerConfig::default();
    // STRESS_MARGIN=10 tries another margin without rebuilding the program.
    // STRESS_SOFT=0.5 tries another soft margin (the lead at which a not quite clear move is taken, in doubt).
    if let Some(soft) = std::env::var("STRESS_SOFT").ok().and_then(|v| v.parse().ok()) {
        config.soft_margin = soft;
    }
    if let Some(margin) = std::env::var("STRESS_MARGIN").ok().and_then(|v| v.parse().ok()) {
        config.margin = margin;
    }
    println!("{:<10} {:<16} {:>6} {:>10} {:>6} {:>7} {:>11}", "conditions", "game", "plies", "by itself", "wrong", "typed", "longest run");
    let dark = Conditions { final_gain: 0.5, final_warmth: 0.15, shadow: 0.4, glare: 0.6, ..Conditions::HARD };
    let noisy = Conditions { noise: 30, ..Conditions::HARD };
    let scenarios: [(&str, Conditions, Option<usize>); 6] = [
        ("easy", Conditions::EASY, None),
        ("drift", Conditions::DRIFT, None),
        ("hard", Conditions::HARD, None),
        ("dark", dark, None),
        ("noisy", noisy, None),
        ("catch-up", Conditions::HARD, Some(5)),
    ];
    // STRESS_ONLY=hard,dark runs only those scenarios.
    let only = std::env::var("STRESS_ONLY").ok();
    let mut all_unsure: Vec<(f32, bool)> = Vec::new();
    let mut all_doubts: Vec<(f32, bool)> = Vec::new();
    for (name, conditions, unseen) in scenarios {
        if only.as_ref().is_some_and(|list| !list.split(',').any(|n| n == name)) {
            continue;
        }
        let mut sum = Outcome::default();
        for game in &games {
            let o = play(game, &conditions, &config, unseen);
            println!(
                "{name:<10} {:<16} {:>6} {:>10} {:>6} {:>7} {:>11}",
                game.name, o.plies, o.auto, o.wrong, o.manual, o.longest_manual_run
            );
            sum.plies += o.plies;
            sum.auto += o.auto;
            sum.wrong += o.wrong;
            sum.manual += o.manual;
            sum.longest_manual_run = sum.longest_manual_run.max(o.longest_manual_run);
            all_unsure.extend(o.unsure.iter().copied());
            all_doubts.extend(o.doubts.iter().copied());
        }
        println!(
            "{name:<10} {:<16} {:>6} {:>9.0}% {:>6} {:>7} {:>11}\n",
            "ALL",
            sum.plies,
            100.0 * sum.auto as f64 / sum.plies as f64,
            sum.wrong,
            sum.manual,
            sum.longest_manual_run
        );
    }
    // Moves taken in doubt: how sure the tracker said it was, and how often it was right.
    if !all_doubts.is_empty() {
        let right = all_doubts.iter().filter(|(_, ok)| *ok).count();
        println!(
            "Taken in doubt ({} times, {:.0}% right): by the certainty the tracker gave",
            all_doubts.len(),
            100.0 * right as f64 / all_doubts.len() as f64
        );
        for (low, high) in [(0.0, 0.5), (0.5, 0.7), (0.7, 0.9), (0.9, 1.01)] {
            let bucket: Vec<_> = all_doubts.iter().filter(|(c, _)| *c >= low && *c < high).collect();
            if bucket.is_empty() {
                continue;
            }
            let right = bucket.iter().filter(|(_, ok)| *ok).count();
            println!(
                "  certainty {low:.1}..{high:.1}: {:>3} times, right {:>3.0}%",
                bucket.len(),
                100.0 * right as f64 / bucket.len() as f64
            );
        }
    }
    // When the tracker was unsure, was its first candidate the move that was played? By how far it led.
    if !all_unsure.is_empty() {
        println!("When unsure ({} times): how often the first candidate was right, by its lead over the second (nats)", all_unsure.len());
        for (low, high) in [(0.0, 1.0), (1.0, 2.0), (2.0, 3.0), (3.0, 4.0), (4.0, 6.0), (6.0, 1000.0)] {
            let bucket: Vec<_> = all_unsure.iter().filter(|(gap, _)| *gap >= low && *gap < high).collect();
            if bucket.is_empty() {
                continue;
            }
            let right = bucket.iter().filter(|(_, ok)| *ok).count();
            println!(
                "  lead {low:>3}..{high:<5}: {:>3} times, first candidate right {:>3.0}%",
                bucket.len(),
                100.0 * right as f64 / bucket.len() as f64
            );
        }
    }
}
