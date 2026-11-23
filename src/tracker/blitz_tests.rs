//! Fast games: moves played a second or two apart, with a hand that reaches, hovers, rests and leaves again.
//!
//! The pictures come 25 a second, on a fake clock, so a whole game runs in seconds. A "hand" is a disc of skin
//! colour that moves over the board (a disc is what a real hand looks like to the squares: something that is none of
//! empty, white or black). What is checked is the list of moves the tracker reports, in order, against the game.

use super::*;
use crate::calibration::{MARGIN_PX, SQUARE_PX};
use crate::game::uci_of;
use crate::synthetic::{self, OPERA_GAME, PieceGrid};
use shakmaty::san::San;
use std::collections::HashMap;

/// How the game is played at the board.
#[derive(Clone)]
struct Play {
    /// The pause after each move before the next hand starts reaching, in seconds (the last value is repeated).
    think: Vec<f32>,
    /// The hand stays still over the piece it is going to lift, and for how long.
    hover: f32,
    /// The hand stays on the square it has put the piece on, and for how long.
    rest: f32,
    /// How far the hand reaches over the squares around the one it works on, in squares.
    reach: f32,
    skin: [u8; 3],
    fps: f32,
    config: TrackerConfig,
}

impl Play {
    fn new(think: f32) -> Play {
        Play { think: vec![think], hover: 0.0, rest: 0.0, reach: 0.3, skin: [150, 120, 110], fps: 25.0, config: TrackerConfig::default() }
    }

    /// How long one move takes the hand, from the start of the reach to the end of the leaving.
    fn hand_time(&self) -> f32 {
        APPROACH + self.hover + CARRY + SETTLE + self.rest + LEAVE
    }

    fn think_after(&self, ply: usize) -> f32 {
        self.think.get(ply).or(self.think.last()).copied().unwrap_or(1.0)
    }
}

const APPROACH: f32 = 0.25;
const CARRY: f32 = 0.30;
const SETTLE: f32 = 0.10;
const LEAVE: f32 = 0.30;
/// Seconds of the starting position before the first move: the tracker learns the squares from it.
const LEAD_IN: f32 = 2.0;

/// What came out of a game.
struct Outcome {
    /// The moves the tracker reported, in order, as UCI.
    reported: Vec<String>,
    /// How many of the game's moves were reported right, in order, before the first one that was not.
    right: usize,
    /// The tracker's position at the end is the game's.
    in_step: bool,
    unsure_frames: usize,
    /// The moves of the game, as UCI.
    truth: Vec<String>,
    /// The longest one picture took the tracker, and how many took more than 40 ms.
    slowest: Duration,
    slow_pictures: usize,
}

impl Outcome {
    fn complete(&self, plies: usize) -> bool {
        self.right == plies && self.reported.len() == plies && self.in_step
    }

    /// The same moves were reported and the position is right, though not necessarily in the order they were played:
    /// when several moves were played between two still pictures, the board does not say which came first.
    fn same_moves(&self) -> bool {
        let (mut a, mut b) = (self.reported.clone(), self.truth.clone());
        a.sort();
        b.sort();
        a == b && self.in_step
    }

    fn summary(&self, plies: usize) -> String {
        format!(
            "{}/{} right{}, {} reported, {}, {} unsure pictures, slowest {} ms ({} over 40 ms){}",
            self.right,
            plies,
            if self.same_moves() && self.right < plies { " (same moves, other order)" } else { "" },
            self.reported.len(),
            if self.in_step { "in step at the end" } else { "off the game at the end" },
            self.unsure_frames,
            self.slowest.as_millis(),
            self.slow_pictures,
            if self.right < self.reported.len() { format!(", first wrong: {:?}", self.reported.get(self.right)) } else { String::new() }
        )
    }
}

fn game_of(sans: &[&str]) -> Vec<Move> {
    let mut position = Chess::default();
    sans.iter()
        .map(|text| {
            let mv = text.parse::<San>().expect("valid SAN").to_move(&position).expect("legal move");
            position.play_unchecked(mv);
            mv
        })
        .collect()
}

/// A game of random legal moves, always the same for a seed.
fn random_game(seed: u64, plies: usize) -> Vec<Move> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut position = Chess::default();
    let mut moves = Vec::new();
    for _ in 0..plies {
        if position.is_game_over() {
            break;
        }
        let legal = position.legal_moves();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let mv = legal[(state % legal.len() as u64) as usize];
        position.play_unchecked(mv);
        moves.push(mv);
    }
    moves
}

fn lerp(a: (f32, f32), b: (f32, f32), u: f32) -> (f32, f32) {
    let u = u.clamp(0.0, 1.0);
    (a.0 + (b.0 - a.0) * u, a.1 + (b.1 - a.1) * u)
}

/// The middle of a square, in squares from the corner of the board (file, rank).
fn centre(square: usize) -> (f32, f32) {
    ((square % 8) as f32 + 0.5, (square / 8) as f32 + 0.5)
}

/// Plays a game at the board and says what the tracker made of it.
fn play(moves: &[Move], how: &Play) -> Outcome {
    let mut positions = vec![Chess::default()];
    for mv in moves {
        let mut next = positions.last().expect("one position").clone();
        next.play_unchecked(*mv);
        positions.push(next);
    }
    let grids: Vec<PieceGrid> = positions.iter().map(synthetic::grid_from_position).collect();
    let mut pictures: HashMap<PieceGrid, RgbImage> = HashMap::new();
    let calibration = synthetic::corners();
    let to_camera = calibration.projection().expect("synthetic corners").invert();

    // When the hand starts on each move.
    let mut starts = Vec::with_capacity(moves.len());
    let mut at = LEAD_IN;
    for ply in 0..moves.len() {
        starts.push(at);
        at += how.hand_time() + how.think_after(ply);
    }
    let total = at + 3.0;

    let mut tracker = Tracker::new(how.config.clone());
    tracker.set_calibration(Some(&calibration));
    tracker.start(positions[0].clone());
    let base = Instant::now();
    let mut reported: Vec<String> = Vec::new();
    let mut unsure_frames = 0;
    let trace = std::env::var("BLITZ_TRACE").is_ok();
    let mut last_text = String::new();
    // How long the tracker took over one picture: at 25 a second a picture must not take more than about 40 ms, or the
    // pictures queue up behind it.
    let mut slowest = (Duration::ZERO, 0.0f32);
    let mut slow_pictures = 0usize;
    let (mut t, dt, mut frame_number) = (0.0f32, 1.0 / how.fps, 0u64);
    while t < total {
        frame_number += 1;
        // The move that is being made now: the last one whose hand has started.
        let active = starts.iter().rposition(|&s| s <= t);
        let mut grid = grids[0];
        let mut hands: Vec<(f32, f32)> = Vec::new();
        if let Some(k) = active {
            let into = t - starts[k];
            let mv = moves[k];
            let (from, to) = (usize::from(mv.from().expect("a move from a square")), usize::from(mv.to()));
            let (before, after) = (grids[k], grids[k + 1]);
            let lift_at = APPROACH + how.hover;
            let put_at = lift_at + CARRY;
            grid = before;
            if into >= lift_at {
                // The piece is in the hand: its square is empty, and so is that of a piece taken en passant.
                grid[from] = None;
                if !mv.is_castle() {
                    for square in 0..64 {
                        if square != from && square != to && before[square].is_some() && after[square].is_none() {
                            grid[square] = None;
                        }
                    }
                }
            }
            if into >= put_at {
                for square in 0..64 {
                    if after[square] != before[square] {
                        grid[square] = after[square];
                    }
                }
            }
            // The hand of this move, and the one of the move before, which may still be leaving.
            for j in k.saturating_sub(1)..=k {
                let into = t - starts[j];
                if !(0.0..=how.hand_time()).contains(&into) {
                    continue;
                }
                let (from, to) = (usize::from(moves[j].from().expect("a move from a square")), usize::from(moves[j].to()));
                let rest_place = (3.5, if positions[j].turn() == Color::White { -3.0 } else { 11.0 });
                let clock = (10.5, 2.0);
                let put_at = APPROACH + how.hover + CARRY;
                hands.push(if into < APPROACH {
                    lerp(rest_place, centre(from), into / APPROACH)
                } else if into < APPROACH + how.hover {
                    centre(from)
                } else if into < put_at {
                    lerp(centre(from), centre(to), (into - APPROACH - how.hover) / CARRY)
                } else if into < put_at + SETTLE + how.rest {
                    centre(to)
                } else {
                    lerp(centre(to), clock, (into - put_at - SETTLE - how.rest) / LEAVE)
                });
            }
        }
        let mut picture = pictures.entry(grid).or_insert_with(|| synthetic::frame_from_grid(&grid)).clone();
        for hand in &hands {
            let (cx, cy) = to_camera * (MARGIN_PX as f32 + hand.0 * SQUARE_PX as f32, MARGIN_PX as f32 + (8.0 - hand.1) * SQUARE_PX as f32);
            let radius = (0.5 + how.reach) * 40.0;
            let (w, h) = (picture.width() as i32, picture.height() as i32);
            for py in (((cy - radius) as i32).max(0))..(((cy + radius) as i32 + 1).min(h)) {
                for px in (((cx - radius) as i32).max(0))..(((cx + radius) as i32 + 1).min(w)) {
                    let (dx, dy) = (px as f32 - cx, py as f32 - cy);
                    if dx * dx + dy * dy <= radius * radius {
                        picture.put_pixel(px as u32, py as u32, image::Rgb(how.skin));
                    }
                }
            }
        }
        synthetic::degrade(&mut picture, -15, 10, frame_number);
        let fed_at = Instant::now();
        let detection = tracker.feed(&picture, base + Duration::from_secs_f32(t));
        let took = fed_at.elapsed();
        if took > slowest.0 {
            slowest = (took, t);
        }
        if took > Duration::from_millis(40) {
            slow_pictures += 1;
        }
        if trace && !matches!(detection, Detection::Nothing) {
            let truth = active.map_or("-".to_string(), |k| format!("{} ({})", k + 1, uci_of(moves[k])));
            let text = match &detection {
                Detection::Move { mv, .. } => format!("MOVE {}", uci_of(*mv)),
                Detection::Sequence { moves, .. } => format!("SEQ {}", moves.iter().map(|m| uci_of(*m)).collect::<Vec<_>>().join(" ")),
                Detection::Unsure { candidates } => {
                    format!(
                        "UNSURE {}",
                        candidates.iter().take(3).map(|c| format!("{} {:.1}", c.uci, c.cost)).collect::<Vec<_>>().join(" | ")
                    )
                }
                Detection::Nothing => String::new(),
            };
            if !text.starts_with("UNSURE") || last_text != text {
                println!("t={t:5.2} ply {truth:<10} {text}");
            }
            last_text = text;
        }
        match detection {
            Detection::Move { mv, .. } => reported.push(uci_of(mv)),
            Detection::Sequence { moves, .. } => reported.extend(moves.iter().map(|m| uci_of(*m))),
            Detection::Unsure { .. } => unsure_frames += 1,
            Detection::Nothing => {}
        }
        t += dt;
    }
    let truth: Vec<String> = moves.iter().map(|m| uci_of(*m)).collect();
    let right = reported.iter().zip(&truth).take_while(|(a, b)| a == b).count();
    let last = positions.last().expect("one position");
    let in_step = tracker.position().board() == last.board() && tracker.position().turn() == last.turn();
    Outcome { reported, right, in_step, unsure_frames, truth, slowest: slowest.0, slow_pictures }
}

fn opera() -> Vec<Move> {
    game_of(&OPERA_GAME)
}

/// Every scenario, one line each, to look at while tuning: `cargo test blitz_matrix -- --ignored --nocapture`.
#[test]
#[ignore = "prints a table for tuning; takes a while"]
fn blitz_matrix() {
    let game = opera();
    let mut rows: Vec<(String, Play)> = Vec::new();
    let mut add = |name: &str, how: Play| rows.push((name.to_string(), how));
    add("normal (pause 2.5 s)", Play::new(2.5));
    add("pause 1.0 s", Play::new(1.0));
    add("pause 0.5 s", Play::new(0.5));
    add("pause 0.2 s", Play::new(0.2));
    add("pause 0.1 s", Play::new(0.1));
    add("pause 0.05 s", Play::new(0.05));
    add("pause 0.0 s", Play::new(0.0));
    add("pause 0.5 s, hand rests 0.5 s", Play { rest: 0.5, ..Play::new(0.5) });
    add("pause 0.5 s, hand hovers 0.3 s", Play { hover: 0.3, ..Play::new(0.5) });
    add("normal, hand rests 0.8 s", Play { rest: 0.8, ..Play::new(2.5) });
    add("5 plies fast then slow", Play { think: vec![0.0, 0.0, 0.0, 0.0, 0.0, 3.0], ..Play::new(3.0) });
    for (name, how) in rows {
        let started = Instant::now();
        let outcome = play(&game, &how);
        println!("{name:<34} {}   ({:.1} s)", outcome.summary(game.len()), started.elapsed().as_secs_f32());
    }
    // Games of random moves: captures, promotions and castling in places no real game has them.
    for seed in 1..=4 {
        let game = random_game(seed, 60);
        for (name, how) in [
            ("random, pause 0.4 s, rests 0.3 s", Play { rest: 0.3, ..Play::new(0.4) }),
            ("random, pause 0.2 s, hovers 0.2 s", Play { hover: 0.2, ..Play::new(0.2) }),
            ("random, pause 0.1 s", Play::new(0.1)),
            ("random, pause 1.5 s, rests 0.8 s", Play { rest: 0.8, ..Play::new(1.5) }),
        ] {
            let started = Instant::now();
            let outcome = play(&game, &how);
            println!("game {seed}: {name:<34} {}   ({:.1} s)", outcome.summary(game.len()), started.elapsed().as_secs_f32());
        }
    }
}

/// One scenario with a trace of what the tracker said and when: `BLITZ_CASE=hover cargo test blitz_one -- --ignored --nocapture`.
#[test]
#[ignore = "prints a trace for tuning"]
fn blitz_one() {
    // SAFETY-free: the variable is only read by this process.
    let case = std::env::var("BLITZ_CASE").unwrap_or_else(|_| "hover".into());
    let how = match case.as_str() {
        "hover" => Play { hover: 0.3, ..Play::new(0.5) },
        "rest" => Play { rest: 0.5, ..Play::new(0.5) },
        "rush" => Play { think: vec![0.0, 0.0, 0.0, 0.0, 0.0, 3.0], ..Play::new(3.0) },
        "zero" => Play::new(0.0),
        other if other.starts_with("random:") => {
            // random:<seed>:<pause>
            Play::new(other.split(':').nth(2).and_then(|v| v.parse().ok()).unwrap_or(0.1))
        }
        _ => Play::new(1.0),
    };
    let game = match case.strip_prefix("random:").and_then(|r| r.split(':').next()).and_then(|s| s.parse().ok()) {
        Some(seed) => random_game(seed, 60),
        None => opera(),
    };
    if std::env::var("BLITZ_TRACE").is_ok() {
        println!("truth: {}", game.iter().map(|m| uci_of(*m)).collect::<Vec<_>>().join(" "));
    }
    let outcome = play(&game, &how);
    println!("{case}: {}", outcome.summary(game.len()));
}

#[test]
fn the_game_is_followed_at_a_slow_pace_with_hands_over_the_board() {
    let game = opera();
    let outcome = play(&game, &Play { rest: 0.8, ..Play::new(2.5) });
    assert!(outcome.complete(game.len()), "{}", outcome.summary(game.len()));
}

#[test]
fn the_game_is_followed_when_a_move_comes_every_second_or_so() {
    let game = opera();
    for think in [1.0, 0.5] {
        let outcome = play(&game, &Play::new(think));
        assert!(outcome.complete(game.len()), "pause {think}: {}", outcome.summary(game.len()));
    }
}

#[test]
fn the_game_is_followed_when_the_hand_stays_on_the_board_between_moves() {
    let game = opera();
    let outcome = play(&game, &Play { rest: 0.5, ..Play::new(0.5) });
    assert!(outcome.complete(game.len()), "{}", outcome.summary(game.len()));
    let outcome = play(&game, &Play { hover: 0.3, ..Play::new(0.5) });
    assert!(outcome.complete(game.len()), "{}", outcome.summary(game.len()));
}

#[test]
fn the_game_is_followed_when_each_move_follows_the_last_at_once() {
    let game = opera();
    // Down to a tenth of a second between the hand leaving and the next one arriving.
    for think in [0.2, 0.1] {
        let outcome = play(&game, &Play::new(think));
        assert!(outcome.complete(game.len()), "pause {think}: {}", outcome.summary(game.len()));
    }
}

#[test]
fn a_game_with_hardly_a_pause_between_the_moves_is_caught_up_with() {
    // With no moment between two hands the board is never still, so some moves come together, in an order that
    // the board cannot tell. The tracker must not stay lost: it ends in step, with every move of the game.
    let game = opera();
    let outcome = play(&game, &Play::new(0.05));
    assert!(outcome.same_moves(), "{}", outcome.summary(game.len()));
}

#[test]
fn a_few_moves_played_in_a_rush_do_not_end_the_tracking() {
    let game = opera();
    let outcome = play(&game, &Play { think: vec![0.0, 0.0, 0.0, 0.0, 0.0, 3.0], ..Play::new(3.0) });
    // The moves played in the rush are found by a longer search, all together; the board cannot say in which order
    // the moves of one side were played, so the order may differ, but the position and the moves are right, the
    // game goes on from there, and the person is told that the moves were taken on thinner evidence.
    assert!(outcome.same_moves(), "{}", outcome.summary(game.len()));
    assert!(outcome.reported.len() == game.len(), "{}", outcome.summary(game.len()));
}

#[test]
fn random_games_are_followed_at_a_fast_pace() {
    for seed in [1, 2, 3] {
        let game = random_game(seed, 60);
        let outcome = play(&game, &Play { rest: 0.3, ..Play::new(0.4) });
        assert!(outcome.complete(game.len()), "game {seed}: {}", outcome.summary(game.len()));
    }
}
