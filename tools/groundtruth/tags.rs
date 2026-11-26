//! Shows what the tags of a recording's pictures are (see `nicochess::tags`): how much of the board changed since the
//! picture before, which pictures are the same as the one before, where a hand or an arm is over the board, and how many
//! looks the program could skip. For tuning the limits of the tags on real recordings. Never changes the recording.
//!
//! `tags <recording folder> [--camera 0] [--truth <truth.json>] [--csv <file>]`
//!
//! It reads the pictures of the camera (as the keyframes tool does), makes a glance of each with the calibration in force,
//! and prints the stretches of occluded pictures, the share of duplicates, and (with a truth) how far each move of the
//! game was from the nearest occluded stretch (a move is made by a hand over the board, so it should be close to one).
//! With `--csv` one line per picture: time, status of the tracker, squares changed, squares covered, duplicate, occluded.

use anyhow::{Context, Result, bail};
use image::RgbImage;
use nicochess::calibration::Calibration;
use nicochess::picture::Picture;
use nicochess::record::SessionMeta;
use nicochess::tags::{OCCLUDED_SQUARES, QUIET_FOR, Tagger};
use nicochess::video::{PictureRef, Pictures};
use nicochess::vision::Sampler;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn read_events(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

fn corners_of(value: &Value) -> Option<Calibration> {
    let corners = value.get("corners")?.as_array()?;
    let mut out = [[0.0f32; 2]; 4];
    for (i, c) in corners.iter().take(4).enumerate() {
        out[i] = [c.get(0)?.as_f64()? as f32, c.get(1)?.as_f64()? as f32];
    }
    Some(Calibration::new(out))
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: tags <recording folder> [--camera 0] [--truth <truth.json>] [--csv <file>]";
    let Some(folder) = args.next() else { bail!(usage) };
    let folder = PathBuf::from(folder);
    let (mut camera, mut truth, mut csv_path) = (0i64, None::<PathBuf>, None::<PathBuf>);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--camera" => camera = args.next().context("--camera needs a number")?.parse()?,
            "--truth" => truth = Some(PathBuf::from(args.next().context("--truth needs a file")?)),
            "--csv" => csv_path = Some(PathBuf::from(args.next().context("--csv needs a file")?)),
            other => bail!("unknown option {other}\n{usage}"),
        }
    }
    let session: SessionMeta = serde_json::from_slice(&std::fs::read(folder.join("session.json")).context("session.json")?)?;
    let events = read_events(&folder.join("events.jsonl"))?;
    let mut calibration = if camera == 0 { session.calibration } else { session.calibration2 }.context("no calibration for the camera")?;
    let adjust: Option<Picture> =
        if session.every_picture { if camera == 0 { Some(session.picture) } else { session.picture2 } } else { None };
    let mut pictures = Pictures::new(&folder);

    struct Shot {
        t: f64,
        status: String,
        picture: PictureRef,
        calibration: Calibration,
    }
    let mut shots: Vec<Shot> = Vec::new();
    let mut look_status = String::new();
    for e in &events {
        match e["type"].as_str() {
            Some("calibration") if e["cam"].as_i64() == Some(camera) => {
                if let Some(c) = e.get("calibration").and_then(corners_of) {
                    calibration = c;
                }
            }
            Some("look") => look_status = e["status"].as_str().unwrap_or_default().to_string(),
            Some("frame") if e["cam"].as_i64() == Some(camera) => {
                if let Some(picture) = PictureRef::of_event(e) {
                    shots.push(Shot {
                        t: e["captured"].as_f64().or_else(|| e["t"].as_f64()).unwrap_or(0.0),
                        status: e["status"].as_str().map_or_else(|| look_status.clone(), str::to_string),
                        picture,
                        calibration,
                    });
                }
            }
            Some("video") => pictures.note_video(e),
            _ => {}
        }
    }
    pictures.require_ffmpeg_for_videos()?;
    if shots.len() < 2 {
        bail!("fewer than two pictures of camera {camera}");
    }

    let base = Instant::now();
    let mut tagger = Tagger::new();
    let mut csv = String::from("t,status,changed,covered,duplicate,occluded\n");
    let mut sampler: Option<(Calibration, Sampler)> = None;
    let (mut duplicates, mut occluded, mut skippable, mut analysed) = (0usize, 0usize, 0usize, 0usize);
    let (mut glance_time, mut tag_time) = (Duration::ZERO, Duration::ZERO);
    let mut stretches: Vec<(f64, f64, u32, usize)> = Vec::new();
    for (i, shot) in shots.iter().enumerate() {
        let image: RgbImage = {
            let image = pictures.load(&shot.picture)?;
            match adjust {
                Some(settings) => settings.adjust(&image).unwrap_or(image),
                None => image,
            }
        };
        if sampler.as_ref().is_none_or(|(c, _)| c != &shot.calibration) {
            let s = Sampler::new(&shot.calibration).context("the corners do not define a board")?;
            sampler = Some((shot.calibration, s));
        }
        let timer = Instant::now();
        let glance = sampler.as_ref().map(|(_, s)| s.glance(&image)).context("no sampler")?;
        glance_time += timer.elapsed();
        let at = base + Duration::from_secs_f64(shot.t.max(0.0));
        let timer = Instant::now();
        let tag = tagger.tag(0, at, &glance);
        tag_time += timer.elapsed();
        // What the program would do while behind, on every picture: skip when it may, else look.
        if tagger.may_skip(&[(0, at, &glance)]) {
            skippable += 1;
            tagger.skipped(0);
        } else {
            analysed += 1;
            tagger.analysed(0, &glance);
        }
        duplicates += usize::from(tag.duplicate);
        occluded += usize::from(tag.occluded);
        writeln!(
            csv,
            "{:.3},{},{},{},{},{}",
            shot.t,
            shot.status,
            tag.changed,
            tag.covered,
            u8::from(tag.duplicate),
            u8::from(tag.occluded)
        )?;
        for o in tagger.take_occlusions() {
            let secs = |x: Instant| x.saturating_duration_since(base).as_secs_f64();
            stretches.push((secs(o.from), secs(o.to), o.pictures, o.peak));
        }
        if i % 500 == 499 {
            eprintln!("  tagged {}", i + 1);
        }
    }
    for o in tagger.take_occlusions() {
        let secs = |x: Instant| x.saturating_duration_since(base).as_secs_f64();
        stretches.push((secs(o.from), secs(o.to), o.pictures, o.peak));
    }
    if let Some(path) = csv_path {
        std::fs::write(&path, csv)?;
    }

    let n = shots.len() as f64;
    let n_pictures = n;
    println!("{}: {} pictures of camera {camera}, {:.0} s", folder.display(), shots.len(), shots.last().map_or(0.0, |s| s.t) - shots[0].t);
    println!(
        "  cost of the tags: a glance takes {:.0} us a picture, the tag itself {:.0} us",
        glance_time.as_secs_f64() * 1e6 / n_pictures,
        tag_time.as_secs_f64() * 1e6 / n_pictures
    );
    println!(
        "  same as the picture before: {duplicates} ({:.0}%); occluded: {occluded} ({:.1}%) in {} stretches (at least {OCCLUDED_SQUARES} squares covered)",
        100.0 * duplicates as f64 / n,
        100.0 * occluded as f64 / n,
        stretches.len()
    );
    println!(
        "  a program that skips the looks it may (board quiet for {} s, same as the last look, at most 8 in a row) would look at {analysed} and skip {skippable} ({:.0}%)",
        QUIET_FOR.as_secs(),
        100.0 * skippable as f64 / n
    );
    for (from, to, pictures, peak) in &stretches {
        println!("  occluded {from:7.2} s to {to:7.2} s  ({pictures} pictures, up to {peak} squares covered)");
    }
    if let Some(path) = truth {
        let truth: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let mut far = 0usize;
        let mut report = String::new();
        let moves = truth["moves"].as_array().cloned().unwrap_or_default();
        for m in &moves {
            let t = m["time"].as_f64().unwrap_or(0.0);
            // The keyframe of a move is taken when the board is still again: the hand was there just before.
            let distance = stretches
                .iter()
                .map(|(from, to, _, _)| {
                    if t < *from {
                        from - t
                    } else if t > *to {
                        t - to
                    } else {
                        0.0
                    }
                })
                .fold(f64::INFINITY, f64::min);
            if distance > 4.0 {
                far += 1;
            }
            let _ = write!(report, " {}:{:.1}", m["kf"].as_str().unwrap_or("?"), distance);
        }
        println!("  moves of the truth: {}, of which {far} are more than 4 s from any occluded stretch", moves.len());
        println!("  distance (s) of each keyframe to the nearest occluded stretch:{report}");
    }
    Ok(())
}
