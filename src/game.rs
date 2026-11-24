//! A recorded game: the moves with their timing, the rules, and the PGN export.
//!
//! The JSON form of `GameRecord` is what is stored on disk. Positions are not
//! stored: they are recomputed by replaying the moves.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use shakmaty::KnownOutcome;
use shakmaty::fen::Fen;
use shakmaty::san::{San, SanPlus};
use shakmaty::uci::UciMove;
use shakmaty::{CastlingMode, Chess, Color, EnPassantMode, Move, Outcome, Position, Role};

/// File format version, bumped when the stored shape changes.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MoveRecord {
    /// Long algebraic notation, for example `e2e4` or `e7e8q`.
    pub uci: String,
    /// Standard algebraic notation, for example `Nf3` or `Qxf7#`.
    pub san: String,
    /// Seconds since the first move of the game (the clock starts with it, so the first move is at 0) when the board
    /// settled after the move.
    pub t: f64,
    /// The promotion piece was assumed (queen) because vision cannot see it.
    #[serde(default)]
    pub promotion_guess: bool,
    /// The move was seen in the same picture as the one before it (two or three moves played between two pictures):
    /// the page shows them all as the last move.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub together: bool,
    /// In a game with a clock: the seconds White and Black have left once this move is made (the extra time of the
    /// move included). Taking a move back goes back to what the move before it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<[f64; 2]>,
}

/// The longest base time and extra time a clock may be given.
pub const MAX_BASE_SECS: f64 = 36_000.0;
pub const MAX_EXTRA_SECS: f64 = 3_600.0;

/// How much time each player has, and how much is added to a player's time after each move. The same for both.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct TimeControl {
    /// Seconds each player has at the start.
    pub base: f64,
    /// Seconds added to the time of a player after each move (0 for none).
    #[serde(default)]
    pub increment: f64,
}

impl TimeControl {
    /// Checks what came from the page: a time of at least a second, and an extra time that is not negative.
    pub fn checked(base: f64, increment: f64) -> Result<TimeControl> {
        if !base.is_finite() || !(1.0..=MAX_BASE_SECS).contains(&base) {
            bail!("the time of each player must be between 1 second and 10 hours");
        }
        if !increment.is_finite() || !(0.0..=MAX_EXTRA_SECS).contains(&increment) {
            bail!("the extra time per move must be between 0 seconds and 1 hour");
        }
        Ok(TimeControl { base: round_tenths(base), increment: round_tenths(increment) })
    }

    /// The PGN `TimeControl` tag: seconds, then `+` and the extra seconds when there are some (`300+3`).
    pub fn pgn(&self) -> String {
        let base = self.base.round() as u64;
        let extra = self.increment.round() as u64;
        if extra > 0 { format!("{base}+{extra}") } else { base.to_string() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GameRecord {
    pub version: u32,
    pub id: String,
    /// Start of the game in UTC, ISO 8601, taken from the user's browser.
    pub started_at: Option<String>,
    pub white: String,
    pub black: String,
    /// `None` for the standard starting position.
    pub start_fen: Option<String>,
    pub moves: Vec<MoveRecord>,
    /// `1-0`, `0-1`, `1/2-1/2`, or `None` while the game is not finished.
    pub result: Option<String>,
    /// How the game ended: checkmate, stalemate, resignation, time forfeit, ...
    pub termination: Option<String>,
    /// The clock of the game, when it was played against one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_control: Option<TimeControl>,
}

pub fn parse_fen(fen: &str) -> Result<Chess> {
    let parsed: Fen = fen.trim().parse().map_err(|e| anyhow!("bad FEN: {e}"))?;
    parsed.into_position(CastlingMode::Standard).map_err(|e| anyhow!("illegal position: {e}"))
}

/// A draw rule about repeated positions or the number of moves without a capture or a pawn move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawRule {
    /// The same position five times: the game is drawn at once.
    Fivefold,
    /// 75 moves each without a capture or a pawn move: drawn at once.
    SeventyFive,
    /// The same position three times: a player may claim the draw.
    Threefold,
    /// 50 moves each without a capture or a pawn move: a player may claim the draw.
    FiftyMove,
}

impl DrawRule {
    pub fn name(self) -> &'static str {
        match self {
            DrawRule::Fivefold => "fivefold repetition",
            DrawRule::SeventyFive => "75-move rule",
            DrawRule::Threefold => "threefold repetition",
            DrawRule::FiftyMove => "50-move rule",
        }
    }

    /// True when the game ends by itself, without a claim.
    pub fn automatic(self) -> bool {
        matches!(self, DrawRule::Fivefold | DrawRule::SeventyFive)
    }
}

/// What makes two positions "the same" for the repetition rules: the first four fields of the
/// FEN (pieces, side to move, castling, en passant), the last two being the move counters.
fn repetition_key(pos: &Chess) -> String {
    fen_of(pos).split(' ').take(4).collect::<Vec<_>>().join(" ")
}

pub fn fen_of(pos: &Chess) -> String {
    Fen::from_position(pos, EnPassantMode::Legal).to_string()
}

/// Reads a move written as UCI (`e2e4`) or SAN (`Nf3`), and checks it is legal.
pub fn parse_move(pos: &Chess, text: &str) -> Result<Move> {
    let text = text.trim();
    if text.is_empty() {
        bail!("empty move");
    }
    if let Ok(uci) = text.parse::<UciMove>() {
        return uci.to_move(pos).map_err(|_| anyhow!("{text} is not legal here"));
    }
    let san = San::from_ascii(text.as_bytes()).map_err(|_| anyhow!("cannot read the move {text}"))?;
    san.to_move(pos).map_err(|_| anyhow!("{text} is not legal here"))
}

pub fn uci_of(mv: Move) -> String {
    UciMove::from_standard(mv).to_string()
}

pub fn san_of(pos: &Chess, mv: Move) -> String {
    SanPlus::from_move(pos.clone(), mv).to_string()
}

/// Cleans a player name: no control characters, limited length, never empty.
pub fn clean_name(name: &str, default: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).collect();
    let cleaned: String = cleaned.trim().chars().take(40).collect();
    if cleaned.is_empty() { default.to_string() } else { cleaned }
}

impl GameRecord {
    pub fn new(id: String, started_at: Option<String>, white: &str, black: &str, start_fen: Option<String>) -> Result<Self> {
        let start_fen = start_fen.map(|f| f.trim().to_string()).filter(|f| !f.is_empty());
        if let Some(fen) = &start_fen {
            parse_fen(fen)?;
        }
        Ok(GameRecord {
            version: FORMAT_VERSION,
            id,
            started_at: started_at.filter(|s| valid_timestamp(s)),
            white: clean_name(white, "White"),
            black: clean_name(black, "Black"),
            start_fen,
            moves: Vec::new(),
            result: None,
            termination: None,
            time_control: None,
        })
    }

    /// The colour that makes the move number `index` (0 is the first move of the game).
    pub fn mover(&self, index: usize) -> Color {
        let first = self.start_position().map_or(Color::White, |p| p.turn());
        if index.is_multiple_of(2) { first } else { first.other() }
    }

    pub fn start_position(&self) -> Result<Chess> {
        match &self.start_fen {
            Some(fen) => parse_fen(fen),
            None => Ok(Chess::default()),
        }
    }

    /// The position after every recorded move.
    pub fn position(&self) -> Result<Chess> {
        let mut pos = self.start_position()?;
        for record in &self.moves {
            let mv = parse_move(&pos, &record.uci).with_context(|| format!("bad stored move {}", record.uci))?;
            pos.play_unchecked(mv);
        }
        Ok(pos)
    }

    /// The piece each move took, one entry per move (`None` for a move that took nothing). A pawn taken en passant counts.
    /// The page adds these up to show what each player has taken. It comes from the rules, not from the notation: the
    /// notation says that a piece was taken, not which one.
    pub fn captures(&self) -> Result<Vec<Option<Role>>> {
        let mut pos = self.start_position()?;
        let mut taken = Vec::with_capacity(self.moves.len());
        for record in &self.moves {
            let mv = parse_move(&pos, &record.uci)?;
            taken.push(mv.capture());
            pos.play_unchecked(mv);
        }
        Ok(taken)
    }

    /// The position at the start, then after each move (so `moves.len() + 1` entries).
    pub fn fens(&self) -> Result<Vec<String>> {
        let mut pos = self.start_position()?;
        let mut fens = vec![fen_of(&pos)];
        for record in &self.moves {
            let mv = parse_move(&pos, &record.uci)?;
            pos.play_unchecked(mv);
            fens.push(fen_of(&pos));
        }
        Ok(fens)
    }

    /// Records a move that must be legal in the current position.
    pub fn push_move(&mut self, mv: Move, t: f64, promotion_guess: bool) -> Result<&MoveRecord> {
        if self.result.is_some() {
            bail!("the game is over");
        }
        let pos = self.position()?;
        if !pos.legal_moves().contains(&mv) {
            bail!("illegal move");
        }
        let t = round_tenths(t.max(self.moves.last().map_or(0.0, |m| m.t)));
        self.moves.push(MoveRecord { uci: uci_of(mv), san: san_of(&pos, mv), t, promotion_guess, together: false, left: None });
        Ok(self.moves.last().expect("just pushed"))
    }

    /// Records a move given as text (UCI or SAN).
    pub fn push_text(&mut self, text: &str, t: f64) -> Result<&MoveRecord> {
        let mv = parse_move(&self.position()?, text)?;
        self.push_move(mv, t, false)
    }

    /// Takes back the last move, and reopens the game if it had ended by the rules.
    pub fn undo(&mut self) -> Option<MoveRecord> {
        let undone = self.moves.pop();
        if undone.is_some() {
            self.result = None;
            self.termination = None;
        }
        undone
    }

    /// Changes the piece the last move promoted to (`q`, `r`, `b` or `n`).
    pub fn set_last_promotion(&mut self, piece: char) -> Result<()> {
        let role = match piece.to_ascii_lowercase() {
            'q' => Role::Queen,
            'r' => Role::Rook,
            'b' => Role::Bishop,
            'n' => Role::Knight,
            other => bail!("cannot promote to {other}"),
        };
        let last = self.moves.pop().ok_or_else(|| anyhow!("no move to change"))?;
        let rebuilt = (|| -> Result<MoveRecord> {
            if last.uci.len() != 5 {
                bail!("the last move is not a promotion");
            }
            let pos = self.position()?;
            let mut uci = last.uci[..4].to_string();
            uci.push(role.char());
            let mv = parse_move(&pos, &uci)?;
            Ok(MoveRecord {
                uci: uci_of(mv),
                san: san_of(&pos, mv),
                t: last.t,
                promotion_guess: false,
                together: last.together,
                left: last.left,
            })
        })();
        match rebuilt {
            Ok(record) => {
                self.moves.push(record);
                self.result = None;
                self.termination = None;
                Ok(())
            }
            Err(e) => {
                self.moves.push(last);
                Err(e)
            }
        }
    }

    /// The strongest repetition or move-count rule that applies after the recorded moves, if any.
    /// Positions are the same when the pieces, the side to move, the castling rights and the
    /// possible en passant captures are the same (the FIDE definition).
    pub fn draw_rule(&self) -> Result<Option<DrawRule>> {
        let mut pos = self.start_position()?;
        let mut keys = vec![repetition_key(&pos)];
        for record in &self.moves {
            let mv = parse_move(&pos, &record.uci).with_context(|| format!("bad stored move {}", record.uci))?;
            pos.play_unchecked(mv);
            keys.push(repetition_key(&pos));
        }
        let now = keys.last().cloned().unwrap_or_default();
        let repeats = keys.iter().filter(|key| **key == now).count();
        let clock = pos.halfmoves();
        Ok(if repeats >= 5 {
            Some(DrawRule::Fivefold)
        } else if clock >= 150 {
            Some(DrawRule::SeventyFive)
        } else if repeats >= 3 {
            Some(DrawRule::Threefold)
        } else if clock >= 100 {
            Some(DrawRule::FiftyMove)
        } else {
            None
        })
    }

    /// The draw a player may claim now (threefold repetition, 50-move rule), if the game is
    /// still going. The automatic ones end the game by themselves (see `rules_outcome`).
    pub fn claimable_draw(&self) -> Result<Option<&'static str>> {
        if self.result.is_some() {
            return Ok(None);
        }
        Ok(self.draw_rule()?.filter(|rule| !rule.automatic()).map(DrawRule::name))
    }

    /// The result and the reason if the rules say the game is over.
    pub fn rules_outcome(&self) -> Result<Option<(String, String)>> {
        let pos = self.position()?;
        Ok(match pos.outcome() {
            // Mate and stalemate come first; then the draws that need no claim.
            Outcome::Unknown => match self.draw_rule()? {
                Some(rule) if rule.automatic() => Some(("1/2-1/2".to_string(), rule.name().to_string())),
                _ => None,
            },
            Outcome::Known(known) => {
                let reason = if pos.legal_moves().is_empty() {
                    if pos.is_check() { "checkmate" } else { "stalemate" }
                } else {
                    "insufficient material"
                };
                let result = match known {
                    KnownOutcome::Decisive { winner: Color::White } => "1-0",
                    KnownOutcome::Decisive { winner: Color::Black } => "0-1",
                    KnownOutcome::Draw => "1/2-1/2",
                };
                Some((result.to_string(), reason.to_string()))
            }
        })
    }

    pub fn finish(&mut self, result: &str, termination: &str) {
        self.result = Some(result.to_string());
        self.termination = Some(termination.to_string());
    }

    /// The game in PGN, with the time spent on each move as `[%emt h:mm:ss]`.
    pub fn to_pgn(&self) -> Result<String> {
        let mut out = String::new();
        let result = self.result.as_deref().unwrap_or("*");
        let (date, time) = pgn_date_time(self.started_at.as_deref());
        let mut tag = |name: &str, value: &str| {
            out.push_str(&format!("[{name} \"{}\"]\n", escape_tag(value)));
        };
        tag("Event", "NicoChess game");
        tag("Site", "NicoChess");
        tag("Date", &date);
        tag("Round", "-");
        tag("White", &self.white);
        tag("Black", &self.black);
        tag("Result", result);
        if let Some(control) = &self.time_control {
            tag("TimeControl", &control.pgn());
        }
        if let Some(t) = &time {
            tag("UTCDate", &date);
            tag("UTCTime", t);
        }
        if let Some(fen) = &self.start_fen {
            tag("SetUp", "1");
            tag("FEN", fen);
        }
        // PGN only knows a few values; the detail stays in the JSON file.
        if let Some(why) = &self.termination {
            tag("Termination", if why == "abandoned" || why == "time forfeit" { why } else { "normal" });
        }
        out.push('\n');

        let mut tokens: Vec<String> = Vec::new();
        let mut pos = self.start_position()?;
        let mut previous_t = 0.0;
        for record in &self.moves {
            let number = pos.fullmoves();
            // Every move has a comment, so black moves always repeat the number.
            tokens.push(match pos.turn() {
                Color::White => format!("{number}."),
                Color::Black => format!("{number}..."),
            });
            tokens.push(record.san.clone());
            let spent = (record.t - previous_t).max(0.0).round() as u64;
            // With a clock, the time the player has left is given too (`[%clk]`, as other chess programs write it).
            let left = record.left.map(|both| both[if pos.turn() == Color::White { 0 } else { 1 }].max(0.0).round() as u64);
            tokens.push(match left {
                Some(s) => format!(
                    "{{[%emt {}:{:02}:{:02}] [%clk {}:{:02}:{:02}]}}",
                    spent / 3600,
                    spent / 60 % 60,
                    spent % 60,
                    s / 3600,
                    s / 60 % 60,
                    s % 60
                ),
                None => format!("{{[%emt {}:{:02}:{:02}]}}", spent / 3600, spent / 60 % 60, spent % 60),
            });
            previous_t = record.t;
            let mv = parse_move(&pos, &record.uci)?;
            pos.play_unchecked(mv);
        }
        tokens.push(result.to_string());

        let mut line = String::new();
        for token in tokens {
            if !line.is_empty() && line.len() + 1 + token.len() > 80 {
                out.push_str(&line);
                out.push('\n');
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&token);
        }
        out.push_str(&line);
        out.push('\n');
        Ok(out)
    }
}

fn round_tenths(t: f64) -> f64 {
    (t * 10.0).round() / 10.0
}

fn escape_tag(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Accepts `YYYY-MM-DDTHH:MM:SS` followed by anything (as `Date.toISOString()` gives).
fn valid_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 19
        && b.len() <= 40
        && b[..19].iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            _ => c.is_ascii_digit(),
        })
}

/// PGN date as `YYYY.MM.DD` and the time as `HH:MM:SS`, or unknown markers.
fn pgn_date_time(started_at: Option<&str>) -> (String, Option<String>) {
    match started_at {
        Some(s) if valid_timestamp(s) => (s[..10].replace('-', "."), Some(s[11..19].to_string())),
        _ => ("????.??.??".to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::OPERA_GAME;

    fn opera() -> GameRecord {
        let mut game = GameRecord::new("test".into(), Some("2026-11-04T10:15:30.000Z".into()), "Morphy", "Duke", None).unwrap();
        for (i, san) in OPERA_GAME.iter().enumerate() {
            game.push_text(san, 5.0 * (i + 1) as f64).unwrap();
        }
        game
    }

    #[test]
    fn plays_the_opera_game_to_checkmate() {
        let game = opera();
        assert_eq!(game.moves.len(), 33);
        assert_eq!(game.moves[0].san, "e4");
        assert_eq!(game.moves[0].uci, "e2e4");
        assert_eq!(game.moves[22].san, "O-O-O");
        assert_eq!(game.moves[22].uci, "e1c1");
        assert_eq!(game.moves[32].san, "Rd8#");
        assert_eq!(game.rules_outcome().unwrap(), Some(("1-0".to_string(), "checkmate".to_string())));
        assert_eq!(game.fens().unwrap().len(), 34);
    }

    #[test]
    fn says_which_piece_each_move_took() {
        // The opera game: 1.e4 e5 2.Nf3 d6 3.d4 Bg4 4.dxe5 Bxf3 5.Qxf3 dxe5 ...
        let game = opera();
        let taken = game.captures().unwrap();
        assert_eq!(taken.len(), game.moves.len());
        let letters: Vec<char> = taken.iter().map(|t| t.map_or('-', Role::char)).collect();
        assert_eq!(&letters[..10], ['-', '-', '-', '-', '-', '-', 'p', 'n', 'b', 'p'], "{letters:?}");
        // Every capture in the notation is a capture here, and no other move is.
        for (record, took) in game.moves.iter().zip(&taken) {
            assert_eq!(record.san.contains('x'), took.is_some(), "{}", record.san);
        }
    }

    #[test]
    fn a_pawn_taken_en_passant_and_a_piece_taken_on_the_promotion_square_count() {
        // White pawn on e5, Black plays d7d5: en passant. Then a white pawn on g7 takes the rook on h8 and promotes.
        let mut game = GameRecord::new("t".into(), None, "", "", Some("4k3/3p4/8/4P3/8/8/8/4K3 b - - 0 1".into())).unwrap();
        game.push_text("d5", 1.0).unwrap();
        game.push_text("exd6", 2.0).unwrap();
        assert_eq!(game.captures().unwrap(), [None, Some(Role::Pawn)]);

        let mut game = GameRecord::new("t".into(), None, "", "", Some("7r/6P1/8/8/8/8/k7/4K3 w - - 0 1".into())).unwrap();
        game.push_text("gxh8=N", 1.0).unwrap();
        assert_eq!(game.captures().unwrap(), [Some(Role::Rook)]);
    }

    #[test]
    fn rejects_illegal_moves_and_moves_after_the_end() {
        let mut game = GameRecord::new("t".into(), None, "", "", None).unwrap();
        assert!(game.push_text("e5", 1.0).is_err(), "pawn cannot jump three squares");
        assert!(game.push_text("Ke2", 1.0).is_err(), "square is occupied");
        assert_eq!((game.white.as_str(), game.black.as_str()), ("White", "Black"));

        let mut finished = opera();
        finished.finish("1-0", "checkmate");
        assert!(finished.push_text("a3", 999.0).is_err());
    }

    #[test]
    fn a_game_with_a_clock_keeps_the_time_left_and_says_so_in_the_pgn() {
        let mut game = GameRecord::new("t".into(), None, "A", "B", None).unwrap();
        game.time_control = Some(TimeControl::checked(300.0, 3.0).unwrap());
        game.push_text("e4", 0.0).unwrap();
        game.moves[0].left = Some([303.0, 300.0]);
        game.push_text("e5", 7.0).unwrap();
        game.moves[1].left = Some([303.0, 296.0]);
        assert_eq!(game.mover(0), Color::White);
        assert_eq!(game.mover(1), Color::Black);
        game.finish("1-0", "time forfeit");
        let pgn = game.to_pgn().unwrap();
        assert!(pgn.contains("[TimeControl \"300+3\"]"), "{pgn}");
        assert!(pgn.contains("[Termination \"time forfeit\"]"), "{pgn}");
        assert!(pgn.contains("1. e4 {[%emt 0:00:00] [%clk 0:05:03]}"), "{pgn}");
        assert!(pgn.contains("1... e5 {[%emt 0:00:07] [%clk 0:04:56]}"), "{pgn}");

        // The clock survives the file, and a game of before the clock existed reads as it did.
        let again: GameRecord = serde_json::from_str(&serde_json::to_string(&game).unwrap()).unwrap();
        assert_eq!(again.time_control, game.time_control);
        assert_eq!(again.moves[1].left, Some([303.0, 296.0]));
        let plain = opera();
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("time_control") && !json.contains("left"), "no clock, nothing added to the file");
        assert!(!plain.to_pgn().unwrap().contains("%clk"));
        let old: GameRecord = serde_json::from_str(&json).unwrap();
        assert!(old.time_control.is_none() && old.moves.iter().all(|m| m.left.is_none()));
    }

    #[test]
    fn a_move_of_a_game_that_starts_with_black_to_move_has_the_right_player() {
        let fen = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";
        let game = GameRecord::new("t".into(), None, "A", "B", Some(fen.into())).unwrap();
        assert_eq!(game.mover(0), Color::Black);
        assert_eq!(game.mover(1), Color::White);
    }

    #[test]
    fn undo_reopens_a_finished_game() {
        let mut game = opera();
        game.finish("1-0", "checkmate");
        assert_eq!(game.undo().unwrap().san, "Rd8#");
        assert!(game.result.is_none());
        assert_eq!(game.moves.len(), 32);
        assert!(game.push_text("Rd8#", 200.0).is_ok());
    }

    #[test]
    fn pgn_has_tags_comments_and_the_result() {
        let mut game = opera();
        game.finish("1-0", "checkmate");
        let pgn = game.to_pgn().unwrap();
        assert!(pgn.contains("[Date \"2026.11.04\"]"), "{pgn}");
        assert!(pgn.contains("[UTCTime \"10:15:30\"]"));
        assert!(pgn.contains("[White \"Morphy\"]"));
        assert!(pgn.contains("[Result \"1-0\"]"));
        assert!(pgn.contains("[Termination \"normal\"]"));
        assert!(!pgn.contains("[FEN"));
        assert!(pgn.contains("1. e4 {[%emt 0:00:05]} 1... e5 {[%emt 0:00:05]} 2. Nf3"), "{pgn}");
        assert!(pgn.contains("17. Rd8# {[%emt 0:00:05]} 1-0"), "{pgn}");
        assert!(pgn.lines().all(|l| l.len() <= 90));
    }

    #[test]
    fn pgn_for_a_game_from_a_position_and_without_date() {
        let fen = "4k3/8/8/8/8/8/8/R3K3 w Q - 0 1";
        let mut game = GameRecord::new("t".into(), Some("garbage".into()), "A", "B", Some(fen.into())).unwrap();
        game.push_text("O-O-O", 3.0).unwrap();
        let pgn = game.to_pgn().unwrap();
        assert!(pgn.contains("[Date \"????.??.??\"]"));
        assert!(!pgn.contains("UTCTime"));
        assert!(pgn.contains("[SetUp \"1\"]") && pgn.contains(&format!("[FEN \"{fen}\"]")));
        assert!(pgn.trim_end().ends_with('*'));
    }

    #[test]
    fn black_to_move_start_numbers_correctly() {
        let fen = "4k3/8/8/8/8/8/8/4K3 b - - 0 7";
        let mut game = GameRecord::new("t".into(), None, "A", "B", Some(fen.into())).unwrap();
        game.push_text("Kd8", 2.0).unwrap();
        assert!(game.to_pgn().unwrap().contains("7... Kd8"));
    }

    #[test]
    fn promotion_can_be_changed() {
        let fen = "4k3/P7/8/8/8/8/8/4K3 w - - 0 1";
        let mut game = GameRecord::new("t".into(), None, "A", "B", Some(fen.into())).unwrap();
        let mv = parse_move(&game.position().unwrap(), "a7a8q").unwrap();
        game.push_move(mv, 4.0, true).unwrap();
        assert_eq!(game.moves[0].san, "a8=Q+");
        assert!(game.moves[0].promotion_guess);

        game.set_last_promotion('n').unwrap();
        assert_eq!(game.moves[0].uci, "a7a8n");
        assert_eq!(game.moves[0].san, "a8=N");
        assert!(!game.moves[0].promotion_guess);

        assert!(game.set_last_promotion('k').is_err());
        let mut plain = GameRecord::new("t".into(), None, "A", "B", None).unwrap();
        plain.push_text("e4", 1.0).unwrap();
        assert!(plain.set_last_promotion('q').is_err());
        assert_eq!(plain.moves.len(), 1, "a failed change leaves the move in place");
    }

    #[test]
    fn rejects_bad_fen_and_cleans_names() {
        assert!(GameRecord::new("t".into(), None, "A", "B", Some("not a fen".into())).is_err());
        assert_eq!(clean_name("  Al\u{7}ice  ", "White"), "Alice");
        assert_eq!(clean_name("x".repeat(100).as_str(), "White").len(), 40);
    }

    #[test]
    fn json_round_trip() {
        let game = opera();
        let text = serde_json::to_string(&game).unwrap();
        let back: GameRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back.moves, game.moves);
        assert_eq!(back.fens().unwrap(), game.fens().unwrap());
    }

    /// Plays knight moves out and back, from ply `from` to ply `to` (4 plies bring the start position back).
    fn shuffle(game: &mut GameRecord, from: usize, to: usize) {
        for ply in from..to {
            game.push_text(["Nf3", "Nf6", "Ng1", "Ng8"][ply % 4], ply as f64).unwrap();
        }
    }

    #[test]
    fn threefold_repetition_can_be_claimed_and_fivefold_ends_the_game() {
        let mut game = GameRecord::new("t".into(), None, "A", "B", None).unwrap();
        shuffle(&mut game, 0, 4);
        assert_eq!(game.claimable_draw().unwrap(), None, "the start position was seen twice");
        shuffle(&mut game, 4, 8);
        assert_eq!(game.claimable_draw().unwrap(), Some("threefold repetition"));
        assert_eq!(game.rules_outcome().unwrap(), None, "a claim is up to the players");
        shuffle(&mut game, 8, 15);
        assert_eq!(game.rules_outcome().unwrap(), None, "the fifth time has not come yet");
        shuffle(&mut game, 15, 16);
        assert_eq!(game.rules_outcome().unwrap(), Some(("1/2-1/2".to_string(), "fivefold repetition".to_string())));
    }

    #[test]
    fn the_fifty_move_rule_is_claimed_and_the_seventy_five_move_rule_ends_the_game() {
        // A rook move is neither a capture nor a pawn move: the clock goes up by one.
        let claim = "4k3/8/8/8/8/8/8/R3K3 w - - 99 80";
        let mut game = GameRecord::new("t".into(), None, "A", "B", Some(claim.into())).unwrap();
        assert_eq!(game.claimable_draw().unwrap(), None, "99 half-moves");
        game.push_text("Ra2", 1.0).unwrap();
        assert_eq!(game.claimable_draw().unwrap(), Some("50-move rule"));
        assert_eq!(game.rules_outcome().unwrap(), None);

        let forced = "4k3/8/8/8/8/8/8/R3K3 w - - 149 100";
        let mut game = GameRecord::new("t".into(), None, "A", "B", Some(forced.into())).unwrap();
        game.push_text("Ra2", 1.0).unwrap();
        assert_eq!(game.rules_outcome().unwrap(), Some(("1/2-1/2".to_string(), "75-move rule".to_string())));

        // A pawn move starts the count again.
        let pawn = "4k3/8/8/8/8/8/4P3/4K3 w - - 120 90";
        let mut game = GameRecord::new("t".into(), None, "A", "B", Some(pawn.into())).unwrap();
        assert_eq!(game.claimable_draw().unwrap(), Some("50-move rule"));
        game.push_text("e4", 1.0).unwrap();
        assert_eq!(game.claimable_draw().unwrap(), None);
    }

    #[test]
    fn a_finished_game_has_nothing_to_claim() {
        let mut game = GameRecord::new("t".into(), None, "A", "B", Some("4k3/8/8/8/8/8/8/R3K3 w - - 110 80".into())).unwrap();
        assert_eq!(game.claimable_draw().unwrap(), Some("50-move rule"));
        game.finish("1/2-1/2", "50-move rule");
        assert_eq!(game.claimable_draw().unwrap(), None);
    }

    #[test]
    fn stalemate_is_a_draw() {
        // Qc7 leaves the black king in the corner with no move and no check.
        let fen = "k7/8/1K6/8/8/8/8/2Q5 w - - 0 1";
        let mut game = GameRecord::new("t".into(), None, "A", "B", Some(fen.into())).unwrap();
        game.push_text("Qc7", 1.0).unwrap();
        assert_eq!(game.rules_outcome().unwrap(), Some(("1/2-1/2".to_string(), "stalemate".to_string())));
    }
}
