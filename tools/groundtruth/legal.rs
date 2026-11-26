//! The ways a game may have gone on from a position, for reading a real game from pictures (see README.md here).
//!
//! `legal <fen> [--plies 3] [--changed e2,e4] [--top 8]` lists the legal sequences of up to `--plies` moves, each with
//! the squares whose content (empty, white, black) would differ from the position given. With `--changed` (the squares
//! that look different in the pictures) the sequences are ranked by how well their squares match, best first. The ranking
//! only helps; the picture decides. Piece types are not looked at: the chess rules give them.
//!
//! One JSON object per line: `{"plies":1,"san":"e4","uci":"e2e4","fen":"...","changed":["e2","e4"],"miss":0}`.
//!
//! `legal <fen> --play "e2e4 e7e5"` plays the moves instead and prints `{"san":"e4 e5","fen_after":"..."}`; a move that is
//! not legal is an error. A reader uses it to write the position after a reading without computing it by hand.

use anyhow::{Context, Result, bail};
use nicochess::game::{fen_of, san_of, uci_of};
use serde_json::json;
use shakmaty::fen::Fen;
use shakmaty::uci::UciMove;
use shakmaty::{CastlingMode, Chess, Color, Position, Square};

fn classes(pos: &Chess) -> [u8; 64] {
    let mut out = [0u8; 64];
    for sq in Square::ALL {
        out[sq as usize] = match pos.board().piece_at(sq) {
            None => 0,
            Some(p) if p.color == Color::White => 1,
            Some(_) => 2,
        };
    }
    out
}

struct Found {
    plies: usize,
    san: Vec<String>,
    uci: Vec<String>,
    fen: String,
    changed: Vec<String>,
    miss: usize,
}

struct Walk<'a> {
    start: [u8; 64],
    target: &'a Option<Vec<String>>,
    san: Vec<String>,
    uci: Vec<String>,
    out: Vec<Found>,
}

impl Walk<'_> {
    fn go(&mut self, pos: &Chess, left: usize) {
        if left == 0 {
            return;
        }
        for mv in pos.legal_moves() {
            let mut next = pos.clone();
            self.san.push(san_of(pos, mv));
            self.uci.push(uci_of(mv));
            next.play_unchecked(mv);
            let now = classes(&next);
            let changed: Vec<String> = (0..64).filter(|&i| now[i] != self.start[i]).map(|i| Square::new(i as u32).to_string()).collect();
            let miss = self
                .target
                .as_ref()
                .map_or(0, |t| changed.iter().filter(|c| !t.contains(c)).count() + t.iter().filter(|c| !changed.contains(c)).count());
            self.out.push(Found { plies: self.san.len(), san: self.san.clone(), uci: self.uci.clone(), fen: fen_of(&next), changed, miss });
            self.go(&next, left - 1);
            self.san.pop();
            self.uci.pop();
        }
    }
}

/// Plays the moves (UCI, separated by spaces) from the position and prints the one answer: the SAN of each, and the
/// position after them. A move that is not legal is an error, so a reader cannot write down an impossible sequence.
fn play_moves(mut pos: Chess, moves: &str) -> Result<()> {
    let mut san = Vec::new();
    for text in moves.split_whitespace() {
        let uci: UciMove = text.parse().with_context(|| format!("{text} is not a move in UCI notation"))?;
        let mv = uci.to_move(&pos).map_err(|_| anyhow::anyhow!("{text} is not legal here ({})", fen_of(&pos)))?;
        san.push(san_of(&pos, mv));
        pos.play_unchecked(mv);
    }
    println!("{}", json!({"san": san.join(" "), "fen_after": fen_of(&pos)}));
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(fen) = args.next() else {
        bail!("usage: legal <fen> [--plies 3] [--changed e2,e4] [--top 8] | legal <fen> --play \"e2e4 e7e5\"")
    };
    let (mut plies, mut top, mut target, mut play) = (1usize, 8usize, None, None::<String>);
    while let Some(flag) = args.next() {
        let mut value = || args.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--plies" => plies = value()?.parse()?,
            "--top" => top = value()?.parse()?,
            "--changed" => target = Some(value()?.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect::<Vec<_>>()),
            "--play" => play = Some(value()?),
            other => bail!("unknown option {other}"),
        }
    }
    let pos: Chess = fen.parse::<Fen>().context("cannot read the FEN")?.into_position(CastlingMode::Standard).context("not a position")?;
    if let Some(moves) = play {
        return play_moves(pos, &moves);
    }
    let mut walk = Walk { start: classes(&pos), target: &target, san: Vec::new(), uci: Vec::new(), out: Vec::new() };
    walk.go(&pos, plies);
    let mut found = walk.out;
    if target.is_some() {
        // The best match first; of equal ones the shorter sequence.
        found.sort_by_key(|f| (f.miss, f.plies));
        // Sequences that end in the same position are one answer: keep the first.
        let mut seen = std::collections::HashSet::new();
        found.retain(|f| seen.insert(f.fen.split(' ').take(4).collect::<Vec<_>>().join(" ")));
        found.truncate(top);
    }
    for f in found {
        println!(
            "{}",
            json!({"plies": f.plies, "san": f.san.join(" "), "uci": f.uci.join(" "), "fen": f.fen, "changed": f.changed, "miss": f.miss})
        );
    }
    Ok(())
}
