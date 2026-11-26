//! Keyframes of a recording, for reading the real game from the pictures (see README.md in this folder).
//!
//! Reads a recording folder (never changes it): session.json, events.jsonl and the pictures of the first camera. Each
//! picture is warped to a top-down board with the calibration that was in force when it was taken (the one in
//! session.json, then the ones the board following wrote as `calibration` events). The board is still when no square
//! changed between a picture and the one before it; a stretch of still pictures gives one keyframe, kept when at least
//! one square differs from the last kept keyframe. The result is a labelled picture of each keyframe, `index.json`
//! (when, which picture, the squares that changed, what the tracker said around then) and `motion.csv` (the numbers the
//! stretches were cut with, to tune the limits).
//!
//! Nothing here looks at the tracker's own opinion to decide what a keyframe is: only at the pictures.
//!
//! Run: `cargo run --release --example keyframes -- <recording> <output folder> [--px 80] [--still 6] [--frames 4]
//! [--square 9] [--side] [--camera 0]`

use anyhow::{Context, Result, bail};
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, GrayImage, ImageEncoder, Luma, Rgb, RgbImage};
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use nicochess::calibration::Calibration;
use nicochess::record::SessionMeta;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Pixels of a square in the pictures used to find the still stretches.
const SCAN_PX: u32 = 32;
/// The part of a square, from each side, that is left out when it is compared (the edge holds the neighbours' pieces).
const EDGE: u32 = 6;

struct Options {
    recording: PathBuf,
    out: PathBuf,
    /// Pixels of a square in the keyframe pictures.
    px: u32,
    /// The most that a square may change between two pictures of a still stretch (grey levels, mean). The noise of the
    /// camera is 3 to 5 (with the contrast of these recordings), a hand over the board 9 and more.
    still: f32,
    /// The fewest pictures in a still stretch.
    frames: usize,
    /// How much a square must change since the last kept keyframe to count as changed (grey levels, mean); two squares
    /// must for a keyframe to be kept. Picture noise is 3 to 6, a white piece on a light square gives 10 to 15, any other
    /// piece 25 to 100.
    square: f32,
    /// Also write the picture of the camera around the board, as it was.
    side: bool,
    camera: i64,
}

fn options() -> Result<Options> {
    let mut args = std::env::args().skip(1);
    let (Some(recording), Some(out)) = (args.next(), args.next()) else {
        bail!("usage: keyframes <recording folder> <output folder> [--px 80] [--still 6] [--frames 4] [--square 9] [--side] [--camera 0]");
    };
    let mut o =
        Options { recording: recording.into(), out: out.into(), px: 80, still: 6.0, frames: 4, square: 9.0, side: false, camera: 0 };
    while let Some(flag) = args.next() {
        let mut value = || args.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--px" => o.px = value()?.parse()?,
            "--still" => o.still = value()?.parse()?,
            "--frames" => o.frames = value()?.parse()?,
            "--square" => o.square = value()?.parse()?,
            "--camera" => o.camera = value()?.parse()?,
            "--side" => o.side = true,
            other => bail!("unknown option {other}"),
        }
    }
    Ok(o)
}

/// Warps a camera picture to the board seen from above, a1 at the bottom left, with `margin` pixels around it.
fn warp(image: &RgbImage, cal: &Calibration, px: u32, margin: u32) -> Option<RgbImage> {
    let (near, far) = (margin as f32, (margin + 8 * px) as f32);
    let c = &cal.corners;
    let from = [(c[0][0], c[0][1]), (c[1][0], c[1][1]), (c[2][0], c[2][1]), (c[3][0], c[3][1])];
    let projection = Projection::from_control_points(from, [(near, far), (far, far), (far, near), (near, near)])?;
    let side = 8 * px + 2 * margin;
    let mut out = RgbImage::new(side, side);
    warp_into(image, projection, Interpolation::Bilinear, Border::Constant(Rgb([0, 0, 0])), &mut out);
    Some(out)
}

fn grey(image: &RgbImage) -> GrayImage {
    GrayImage::from_fn(image.width(), image.height(), |x, y| {
        let p = image.get_pixel(x, y);
        Luma([((u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114) / 1000) as u8])
    })
}

/// How much each square changed between two scans: the mean difference of the pixels inside it, row 0 being rank 8.
fn square_changes(a: &GrayImage, b: &GrayImage) -> [f32; 64] {
    let mut out = [0.0f32; 64];
    for row in 0..8u32 {
        for col in 0..8u32 {
            let mut sum = 0u64;
            for y in (row * SCAN_PX + EDGE)..((row + 1) * SCAN_PX - EDGE) {
                for x in (col * SCAN_PX + EDGE)..((col + 1) * SCAN_PX - EDGE) {
                    sum += u64::from(a.get_pixel(x, y)[0].abs_diff(b.get_pixel(x, y)[0]));
                }
            }
            let n = (SCAN_PX - 2 * EDGE) * (SCAN_PX - 2 * EDGE);
            out[(row * 8 + col) as usize] = sum as f32 / n as f32;
        }
    }
    out
}

fn square_name(index: usize) -> String {
    format!("{}{}", (b'a' + (index % 8) as u8) as char, 8 - index / 8)
}

struct Shot {
    t: f64,
    file: String,
    feed: i64,
    status: String,
    calibration: Calibration,
}

/// 5x7 letters and digits for the labels around the board (each row is 5 bits).
fn glyph(c: char) -> [u8; 7] {
    match c {
        'a' => [0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111],
        'b' => [0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b11110],
        'c' => [0b00000, 0b00000, 0b01110, 0b10000, 0b10000, 0b10001, 0b01110],
        'd' => [0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10001, 0b01111],
        'e' => [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110],
        'f' => [0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000],
        'g' => [0b00000, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
        'h' => [0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        _ => [0; 7],
    }
}

/// Draws `c` centred on (cx, cy), each pixel of the glyph a `scale` x `scale` block.
fn draw_char(image: &mut RgbImage, c: char, cx: i64, cy: i64, scale: i64) {
    let rows = glyph(c);
    let (x0, y0) = (cx - 5 * scale / 2, cy - 7 * scale / 2);
    for (r, bits) in rows.iter().enumerate() {
        for col in 0..5 {
            if bits >> (4 - col) & 1 == 1 {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let (x, y) = (x0 + col * scale + dx, y0 + r as i64 * scale + dy);
                        if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
                            image.put_pixel(x as u32, y as u32, Rgb([255, 235, 120]));
                        }
                    }
                }
            }
        }
    }
}

/// The board with the files (a to h) under it and above it and the ranks (1 to 8) beside it, white at the bottom.
fn labelled(image: &mut RgbImage, px: u32, margin: u32) {
    let scale = (margin as i64 / 12).max(1);
    for i in 0..8u32 {
        let centre = (margin + i * px + px / 2) as i64;
        let file = (b'a' + i as u8) as char;
        let rank = char::from_digit(8 - i, 10).unwrap_or('?');
        let far = (margin + 8 * px) as i64 + margin as i64 / 2;
        draw_char(image, file, centre, far, scale);
        draw_char(image, file, centre, margin as i64 / 2, scale);
        draw_char(image, rank, margin as i64 / 2, centre, scale);
        draw_char(image, rank, (margin + 8 * px) as i64 + margin as i64 / 2, centre, scale);
    }
}

fn save_jpeg(image: &RgbImage, path: &Path, quality: u8) -> Result<()> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgb8,
    )?;
    fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))
}

fn read_events(path: &Path) -> Result<Vec<Value>> {
    let text = fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    // A last line cut short by a stop is skipped, as the replay does.
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

/// The events that tell what the tracker decided and what the people did, in a short form.
fn short(event: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in ["t", "type", "san", "moves", "text", "fen", "doubt"] {
        if let Some(v) = event.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(c) = event.get("candidates").and_then(Value::as_array) {
        out.insert("candidates".into(), json!(c.iter().take(3).map(|x| x["san"].clone()).collect::<Vec<_>>()));
    }
    Value::Object(out)
}

fn main() -> Result<()> {
    let o = options()?;
    let session: SessionMeta = serde_json::from_slice(&fs::read(o.recording.join("session.json")).context("session.json")?)?;
    let events = read_events(&o.recording.join("events.jsonl"))?;
    fs::create_dir_all(&o.out)?;

    // The pictures of the camera, each with the calibration in force.
    let mut calibration = session.calibration.context("the recording has no calibration")?;
    let mut shots: Vec<Shot> = Vec::new();
    for e in &events {
        match e["type"].as_str() {
            Some("calibration") if e["cam"].as_i64() == Some(o.camera) => {
                if let Some(c) = e.get("calibration").and_then(corners_of) {
                    calibration = c;
                }
            }
            Some("frame") if e["cam"].as_i64() == Some(o.camera) => shots.push(Shot {
                t: e["t"].as_f64().unwrap_or(0.0),
                file: e["file"].as_str().unwrap_or_default().to_string(),
                feed: e["feed"].as_i64().unwrap_or(0),
                status: e["status"].as_str().unwrap_or_default().to_string(),
                calibration,
            }),
            _ => {}
        }
    }
    if shots.len() < 2 {
        bail!("fewer than two pictures of camera {}", o.camera);
    }
    eprintln!("{} pictures of camera {}", shots.len(), o.camera);

    // Pass 1: how much the board changed from one picture to the next.
    let mut motion = vec![0.0f32; shots.len()];
    let mut previous: Option<GrayImage> = None;
    let mut csv = String::from("t,status,motion\n");
    for (i, shot) in shots.iter().enumerate() {
        let image = image::open(o.recording.join(&shot.file)).with_context(|| shot.file.clone())?.to_rgb8();
        let scan = grey(&warp(&image, &shot.calibration, SCAN_PX, 0).context("the corners do not define a board")?);
        if let Some(p) = &previous {
            motion[i] = square_changes(p, &scan).into_iter().fold(0.0, f32::max);
        }
        writeln!(csv, "{},{},{:.2}", shot.t, shot.status, motion[i])?;
        previous = Some(scan);
        if i % 500 == 499 {
            eprintln!("  scanned {}", i + 1);
        }
    }
    fs::write(o.out.join("motion.csv"), csv)?;

    // Stretches of still pictures: one candidate keyframe in the middle of each.
    let mut stretches: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    // (The picture after the last one counts as moving, so that the last stretch is closed.)
    let stills = motion.iter().skip(1).map(|&m| m <= o.still).chain([false]);
    for (i, still) in (1..).zip(stills) {
        match (still, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s >= o.frames {
                    stretches.push((s, i - 1));
                }
                start = None;
            }
            _ => {}
        }
    }

    // Pass 2: keep the candidates that differ from the last kept keyframe, and write them.
    let mut index: Vec<Value> = Vec::new();
    let mut last_kept: Option<(GrayImage, f64)> = None;
    let mut skipped = 0;
    let margin = (o.px * 2 / 5).max(24);
    for (s, e) in &stretches {
        let i = (s + e) / 2;
        let shot = &shots[i];
        let image = image::open(o.recording.join(&shot.file))?.to_rgb8();
        let scan = grey(&warp(&image, &shot.calibration, SCAN_PX, 0).context("bad corners")?);
        let mut strongest: Vec<(String, f32)> = Vec::new();
        let (changed, since) = match &last_kept {
            None => (Vec::new(), 0.0),
            Some((kept, at)) => {
                let changes = square_changes(kept, &scan);
                // A move changes two squares at least, so one square alone changing (a reflection, a shadow) is not one.
                if changes.iter().filter(|&&c| c >= o.square).count() < 2 {
                    skipped += 1;
                    continue;
                }
                // The ten squares that changed most, with how much, to tune `--square` (a white piece on a light
                // square changes little).
                let mut ranked: Vec<(usize, f32)> = changes.iter().copied().enumerate().collect();
                ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
                strongest = ranked.iter().take(10).map(|(n, c)| (square_name(*n), (c * 10.0).round() / 10.0)).collect();
                (changes.iter().enumerate().filter(|(_, c)| **c >= o.square).map(|(n, _)| square_name(n)).collect(), *at)
            }
        };
        let id = format!("kf{:03}", index.len());
        let mut crop = warp(&image, &shot.calibration, o.px, margin).context("bad corners")?;
        labelled(&mut crop, o.px, margin);
        save_jpeg(&crop, &o.out.join(format!("{id}.jpg")), 90)?;
        let mut side_file = Value::Null;
        if o.side {
            let c = &shot.calibration.corners;
            let (xs, ys): (Vec<f32>, Vec<f32>) = (c.iter().map(|p| p[0]).collect(), c.iter().map(|p| p[1]).collect());
            let pad = 0.08 * (xs.iter().cloned().fold(0.0, f32::max) - xs.iter().cloned().fold(f32::MAX, f32::min));
            let x0 = (xs.iter().cloned().fold(f32::MAX, f32::min) - pad).max(0.0) as u32;
            let y0 = (ys.iter().cloned().fold(f32::MAX, f32::min) - pad).max(0.0) as u32;
            let x1 = ((xs.iter().cloned().fold(0.0, f32::max) + pad) as u32).min(image.width());
            let y1 = ((ys.iter().cloned().fold(0.0, f32::max) + pad) as u32).min(image.height());
            let part = image::imageops::crop_imm(&image, x0, y0, x1 - x0, y1 - y0).to_image();
            let name = format!("{id}_side.jpg");
            save_jpeg(&part, &o.out.join(&name), 88)?;
            side_file = json!(name);
        }
        let between: Vec<Value> = events
            .iter()
            .filter(|ev| {
                let t = ev["t"].as_f64().unwrap_or(-1.0);
                t > since && t <= shot.t && !matches!(ev["type"].as_str(), Some("frame" | "views" | "calibration" | "closed"))
            })
            .map(short)
            .collect();
        // What the tracker believed at this moment: the position after its last decision.
        let believed = events
            .iter()
            .filter(|ev| ev["t"].as_f64().unwrap_or(f64::MAX) <= shot.t && ev["fen"].is_string())
            .map(|ev| ev["fen"].clone())
            .next_back()
            .unwrap_or(Value::Null);
        index.push(json!({
            "id": id,
            "t": shot.t,
            "picture": shot.file,
            "feed": shot.feed,
            "tracker_status": shot.status,
            "still_pictures": e - s + 1,
            "still_from": shots[*s].t,
            "still_to": shots[*e].t,
            "changed_squares": changed,
            "strongest_changes": strongest,
            "events_since_previous_keyframe": between,
            "tracker_position": believed,
            "crop": format!("{id}.jpg"),
            "side": side_file,
        }));
        last_kept = Some((scan, shot.t));
    }
    let report = json!({
        "recording": o.recording.display().to_string(),
        "program": session.program,
        "camera": o.camera,
        "start_fen": session.start_fen,
        "pictures": shots.len(),
        "still_stretches": stretches.len(),
        "skipped_same_as_last_keyframe": skipped,
        "settings": {"px": o.px, "still": o.still, "frames": o.frames, "square": o.square},
        "keyframes": index,
    });
    fs::write(o.out.join("index.json"), serde_json::to_string_pretty(&report)?)?;
    eprintln!(
        "{} still stretches, {} skipped as the same board, {} keyframes in {}",
        stretches.len(),
        skipped,
        index.len(),
        o.out.display()
    );
    Ok(())
}
