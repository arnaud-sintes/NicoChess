//! Shows how the pictures of a recording got to the tracker, window by window, so that a moment where the program
//! went wrong can be matched with a moment where it fell behind.
//!
//! The recording notes a `pipe` line for every look of the tracker (how long the look took, how old each picture was,
//! how many pictures came and were not looked at, the time since the previous look) and sums the game up in the
//! `pipeline` field of its last line. This reads them back, in windows of a few seconds, with the moves and the unsure
//! stretches of the same window beside them, and marks the windows that were behind (a picture older than `--age` ms
//! when the tracker started on it, or more than `--gap` ms between two looks).
//!
//!   cargo run --release --example pipeline -- <recording folder> [--window 10] [--age 500] [--gap 300]
//!
//! A recording made before these lines existed has none: the tool says so and shows what the `frame` lines tell (the
//! pictures that were kept and the gaps between them), which is the same thing when every picture was kept.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Default)]
struct Window {
    looks: u64,
    step_sum: f64,
    step_max: f64,
    lock_sum: f64,
    lock_max: f64,
    age_sum: f64,
    age_n: u64,
    age_max: f64,
    /// Pictures, in any view, that were older or waited longer than the limits.
    over_limits: u64,
    skipped: u64,
    gap_max: f64,
    capture_gap_max: f64,
    wait_min: Option<f64>,
    kept: u64,
    kept_gap_max: f64,
    moves: u64,
    unsure: u64,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut folder: Option<PathBuf> = None;
    let (mut width, mut age_limit, mut gap_limit) = (10.0_f64, 500.0_f64, 300.0_f64);
    while let Some(arg) = args.next() {
        let mut number = |what: &str| -> f64 {
            args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                eprintln!("{what} needs a number");
                std::process::exit(2)
            })
        };
        match arg.as_str() {
            "--window" => width = number("--window").max(1.0),
            "--age" => age_limit = number("--age"),
            "--gap" => gap_limit = number("--gap"),
            _ if arg.starts_with("--") => {
                eprintln!("unknown option {arg}");
                std::process::exit(2);
            }
            _ => folder = Some(PathBuf::from(arg)),
        }
    }
    let Some(folder) = folder else {
        eprintln!("usage: pipeline <recording folder> [--window seconds] [--age ms] [--gap ms]");
        std::process::exit(2);
    };
    let text = match std::fs::read_to_string(folder.join("events.jsonl")) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("cannot read {}: {e}", folder.join("events.jsonl").display());
            std::process::exit(1);
        }
    };
    let program = std::fs::read(folder.join("session.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|session| session["program"].as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into());

    let mut windows: BTreeMap<u64, Window> = BTreeMap::new();
    let mut closed: Option<Value> = None;
    let mut pipes = 0u64;
    let mut last_kept: Option<f64> = None;
    let mut last_feed_number: Option<u64> = None;
    for line in text.lines() {
        // A line cut short by a stop is skipped.
        let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
        let (Some(t), Some(kind)) = (event["t"].as_f64(), event["type"].as_str()) else { continue };
        let key = (t / width).floor().max(0.0) as u64;
        let w = windows.entry(key).or_default();
        match kind {
            "pipe" => {
                pipes += 1;
                w.looks += 1;
                let step = event["step_ms"].as_f64().unwrap_or(0.0);
                let lock = event["lock_ms"].as_f64().unwrap_or(0.0);
                w.step_sum += step;
                w.step_max = w.step_max.max(step);
                w.lock_sum += lock;
                w.lock_max = w.lock_max.max(lock);
                // Behind in any view (camera 2 and the depth camera count: the tracker looks at them together).
                for view in event["views"].as_array().into_iter().flatten() {
                    if view["age_ms"].as_f64().unwrap_or(0.0) > age_limit || view["since_last_ms"].as_f64().unwrap_or(0.0) > gap_limit {
                        w.over_limits += 1;
                    }
                }
                // The columns are those of the first camera, the one that matters most for the board.
                if let Some(view) = event["views"].as_array().and_then(|v| v.iter().find(|v| v["cam"] == 0)) {
                    let age = view["age_ms"].as_f64().unwrap_or(0.0);
                    w.age_sum += age;
                    w.age_n += 1;
                    w.age_max = w.age_max.max(age);
                    w.skipped += view["skipped"].as_u64().unwrap_or(0);
                    w.gap_max = w.gap_max.max(view["since_last_ms"].as_f64().unwrap_or(0.0));
                    w.capture_gap_max = w.capture_gap_max.max(view["gap_ms"].as_f64().unwrap_or(0.0));
                    if let Some(wait) = view["wait_ms"].as_f64() {
                        w.wait_min = Some(w.wait_min.map_or(wait, |m| m.min(wait)));
                    }
                }
            }
            "frame" => {
                // One look gives one `feed` number to all its pictures.
                let feed = event["feed"].as_u64();
                if event["cam"].as_u64().unwrap_or(0) == 0 && feed != last_feed_number {
                    last_feed_number = feed;
                    w.kept += 1;
                    if let Some(last) = last_kept {
                        w.kept_gap_max = w.kept_gap_max.max((t - last) * 1000.0);
                    }
                    last_kept = Some(t);
                }
            }
            "detect" => w.moves += event["moves"].as_array().map_or(1, |m| m.len() as u64),
            "unsure" => w.unsure += 1,
            "closed" => closed = Some(event),
            _ => {}
        }
    }

    println!("{} (program {program}), windows of {width} s", folder.display());
    if pipes == 0 {
        println!("This recording has no `pipe` lines: it was made before the pipeline figures existed (or not while playing).");
        println!("What the `frame` lines say: pictures of camera 1 that were kept, and the longest gap between two of them.");
        println!(
            "With --record-all every picture the tracker looked at is kept, so these are its looks; otherwise the gaps are the recording's."
        );
        println!();
        println!("{:>8} {:>6} {:>7} {:>10} {:>6} {:>7}", "from s", "kept", "per s", "max gap ms", "moves", "unsure");
        for (key, w) in &windows {
            if w.kept == 0 && w.moves == 0 && w.unsure == 0 {
                continue;
            }
            println!(
                "{:>8.0} {:>6} {:>7.1} {:>10.0} {:>6} {:>7}",
                *key as f64 * width,
                w.kept,
                w.kept as f64 / width,
                w.kept_gap_max,
                w.moves,
                w.unsure
            );
        }
        return;
    }

    println!(
        "Behind: a picture older than {age_limit} ms when the tracker started on it, or more than {gap_limit} ms between two looks of a view (any view; the columns are camera 1's)."
    );
    println!(
        "age = from the camera to the start of the look; skipped = pictures that came and were not looked at; wait min = the shortest time the"
    );
    println!(
        "capture thread waited for a picture (near 0: the picture was already there, the program was not keeping up); cam gap = the longest"
    );
    println!("time between two pictures as the camera made them (a gap of several pictures: the camera or its driver lost some).");
    println!();
    println!(
        "{:>7} {:>5} {:>6} {:>7} {:>7} {:>8} {:>8} {:>7} {:>7} {:>8} {:>8} {:>5} {:>6} {:>7}",
        "from s",
        "looks",
        "per s",
        "step ms",
        "max",
        "lock max",
        "age ms",
        "age max",
        "skipped",
        "max gap",
        "cam gap",
        "wait",
        "moves",
        "unsure"
    );
    let mut behind = 0;
    for (key, w) in &windows {
        if w.looks == 0 && w.moves == 0 && w.unsure == 0 {
            continue;
        }
        let is_behind = w.over_limits > 0;
        behind += usize::from(is_behind);
        let mean = |sum: f64, n: u64| if n == 0 { 0.0 } else { sum / n as f64 };
        println!(
            "{:>7.0} {:>5} {:>6.1} {:>7.1} {:>7.0} {:>8.0} {:>8.0} {:>7.0} {:>7} {:>8.0} {:>8.0} {:>5.0} {:>6} {:>7} {}",
            *key as f64 * width,
            w.looks,
            w.looks as f64 / width,
            mean(w.step_sum, w.looks),
            w.step_max,
            w.lock_max,
            mean(w.age_sum, w.age_n),
            w.age_max,
            w.skipped,
            w.gap_max,
            w.capture_gap_max,
            w.wait_min.unwrap_or(0.0),
            w.moves,
            w.unsure,
            if is_behind { "BEHIND" } else { "" }
        );
    }
    println!();
    println!("{behind} windows of {} behind.", windows.values().filter(|w| w.looks > 0).count());
    match closed.as_ref().and_then(|c| c.get("pipeline")) {
        Some(summary) => println!("The game as a whole (the `closed` line): {}", serde_json::to_string_pretty(summary).unwrap_or_default()),
        None => println!("The recording has no summary in its last line (it was not closed properly, or it predates it)."),
    }
}
