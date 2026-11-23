//! The chess clock of a game: how much time each player has left, whose time is running, and what a move, a press of
//! the clock (the button or the space bar of the page) and a pause do to it.
//!
//! The clock is a plain value with no thread and no timer of its own. The engine tells it the time of every event, and
//! asks it for the time left at any moment, so every rule here is tested with times made up on the spot.
//!
//! The rules, in short:
//! - The clock starts with the first press, or with the first move that is seen, whichever comes first. A player who
//!   moves before anybody pressed has used no time for that move.
//! - A press hands the time over: the clock of the player who was running stops and the other one runs. The time of a
//!   press is exact; the time a move is seen is the time the board became still, a little after the move.
//! - A move that is seen puts the clock right whatever was pressed: it adds the extra time to the player who moved,
//!   and the player to move is the one whose time runs. So a press before the move is seen and no press at all end in
//!   the same place, and a press that was a mistake is mended by the next move.
//! - While the clock is paused nothing runs and nothing is added.

use crate::game::{GameRecord, TimeControl};
use serde_json::{Value, json};
use shakmaty::{Color, Position};
use std::time::Instant;

fn index(color: Color) -> usize {
    if color == Color::White { 0 } else { 1 }
}

fn name(color: Color) -> &'static str {
    if color == Color::White { "white" } else { "black" }
}

#[derive(Debug, Clone)]
pub struct Clock {
    control: TimeControl,
    /// The seconds White and Black had at `at`; the one that is running has had the time since `at` taken from it.
    left: [f64; 2],
    at: Instant,
    running: Option<Color>,
    /// The first press or the first move has come.
    started: bool,
    paused: bool,
}

impl Clock {
    /// A clock that has not started: both players have all their time.
    pub fn new(control: TimeControl, now: Instant) -> Clock {
        Clock { control, left: [control.base; 2], at: now, running: None, started: false, paused: false }
    }

    /// The clock of a game as its moves say: what each player had left after the last move. It runs (for the player to
    /// move) when `live`, the game is not over and the game has begun. Used to go on with a game after a move was taken
    /// back or after a restart, and to show the time left in a game that is not being played.
    pub fn from_game(game: &GameRecord, now: Instant, paused: bool, live: bool) -> Option<Clock> {
        let control = game.time_control?;
        let left = game.moves.last().and_then(|m| m.left).unwrap_or([control.base; 2]);
        let started = !game.moves.is_empty();
        let mut clock = Clock { control, left, at: now, running: None, started, paused: paused && started };
        if live && started && !clock.paused && game.result.is_none() {
            clock.running = game.position().ok().map(|p| p.turn());
        }
        Some(clock)
    }

    pub fn control(&self) -> TimeControl {
        self.control
    }

    pub fn running(&self) -> Option<Color> {
        self.running
    }

    pub fn started(&self) -> bool {
        self.started
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Takes the time since the last event from the player whose time is running.
    fn settle(&mut self, now: Instant) {
        if let Some(side) = self.running {
            let spent = now.saturating_duration_since(self.at).as_secs_f64();
            self.left[index(side)] = (self.left[index(side)] - spent).max(0.0);
        }
        // An event that is told late about a moment before the last one does not take the clock back.
        self.at = self.at.max(now);
    }

    /// What each player has left at `now` (White, then Black).
    pub fn left(&self, now: Instant) -> [f64; 2] {
        let mut left = self.left;
        if let Some(side) = self.running {
            left[index(side)] = (left[index(side)] - now.saturating_duration_since(self.at).as_secs_f64()).max(0.0);
        }
        left
    }

    /// The player whose time has run out, if there is one.
    pub fn flagged(&self, now: Instant) -> Option<Color> {
        self.running.filter(|side| self.left(now)[index(*side)] <= 0.0)
    }

    /// The time of `side` has run out: it stays at nothing and the clock stops. Returns the player, or `None` if no
    /// time has run out.
    pub fn expire(&mut self, now: Instant) -> Option<Color> {
        let side = self.flagged(now)?;
        self.settle(now);
        self.left[index(side)] = 0.0;
        self.running = None;
        Some(side)
    }

    /// The button or the space bar. Before the start it starts the clock for the player to move (`turn`); paused, it
    /// goes on; running, it hands the time over to the other player.
    pub fn press(&mut self, turn: Color, now: Instant) {
        if self.paused {
            self.resume(turn, now);
        } else if let Some(side) = self.running {
            self.settle(now);
            self.running = Some(side.other());
        } else if !self.started {
            self.started = true;
            self.at = now;
            self.running = Some(turn);
        }
    }

    /// Stops the time of both players until `resume`. Only a clock that has started can be paused.
    pub fn pause(&mut self, now: Instant) -> bool {
        if !self.started || self.paused {
            return false;
        }
        self.settle(now);
        self.running = None;
        self.paused = true;
        true
    }

    /// Goes on after a pause, with the time of the player to move (`turn`) running.
    pub fn resume(&mut self, turn: Color, now: Instant) -> bool {
        if !self.paused {
            return false;
        }
        self.paused = false;
        self.at = self.at.max(now);
        self.running = Some(turn);
        true
    }

    /// The moves of the players `movers`, in the order they were made, were seen at `at`; `turn` is the player to move
    /// after them. Gives what each player had left after each move (to keep with the move).
    pub fn moved(&mut self, movers: &[Color], turn: Color, at: Instant) -> Vec<[f64; 2]> {
        // What is kept with a move is rounded to a tenth of a second: it goes in the file of the game.
        let tenth = |s: f64| (s * 10.0).round() / 10.0;
        if self.paused {
            return vec![[tenth(self.left[0]), tenth(self.left[1])]; movers.len()];
        }
        self.settle(at);
        self.started = true;
        let snapshots = movers
            .iter()
            .map(|mover| {
                self.left[index(*mover)] += self.control.increment;
                [tenth(self.left[0]), tenth(self.left[1])]
            })
            .collect();
        self.running = Some(turn);
        snapshots
    }

    /// The game is over or stopped: no time runs.
    pub fn stop(&mut self, now: Instant) {
        self.settle(now);
        self.running = None;
    }

    /// Makes the player to move the one whose time runs, after the game was changed in a way that may have changed
    /// whose turn it is (a doubtful move replaced by another). Nothing changes while paused or before the start.
    pub fn follow_turn(&mut self, turn: Color, now: Instant) {
        if self.paused || !self.started {
            return;
        }
        self.settle(now);
        self.running = Some(turn);
    }

    /// For the page: the time left at `now`, whose time runs, and whether the clock has started or is paused.
    pub fn to_json(&self, now: Instant) -> Value {
        let left = self.left(now);
        json!({
            "base": self.control.base,
            "increment": self.control.increment,
            "left": [(left[0] * 10.0).round() / 10.0, (left[1] * 10.0).round() / 10.0],
            "running": self.running.map(name),
            "started": self.started,
            "paused": self.paused,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    fn control(base: f64, increment: f64) -> TimeControl {
        TimeControl::checked(base, increment).unwrap()
    }

    #[test]
    fn a_clock_that_has_not_started_keeps_all_the_time() {
        let t0 = Instant::now();
        let clock = Clock::new(control(300.0, 3.0), t0);
        assert_eq!(clock.left(t0 + secs(50.0)), [300.0, 300.0]);
        assert_eq!(clock.running(), None);
        assert!(!clock.started());
        assert_eq!(clock.flagged(t0 + secs(5000.0)), None);
    }

    #[test]
    fn the_first_press_starts_the_player_to_move_and_the_next_ones_hand_the_time_over() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(300.0, 0.0), t0);
        clock.press(Color::White, t0 + secs(1.0));
        assert_eq!(clock.running(), Some(Color::White));
        // White thinks for ten seconds, then presses.
        clock.press(Color::White, t0 + secs(11.0));
        assert_eq!(clock.running(), Some(Color::Black));
        let left = clock.left(t0 + secs(14.0));
        assert!((left[0] - 290.0).abs() < 1e-6, "{left:?}");
        assert!((left[1] - 297.0).abs() < 1e-6, "{left:?}");
    }

    #[test]
    fn a_move_adds_the_extra_time_and_gives_the_time_to_the_other_player() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(300.0, 3.0), t0);
        clock.press(Color::White, t0);
        // White's move is seen after 12 seconds.
        let snapshots = clock.moved(&[Color::White], Color::Black, t0 + secs(12.0));
        assert_eq!(snapshots.len(), 1);
        assert!((snapshots[0][0] - 291.0).abs() < 1e-6, "300 - 12 + 3: {snapshots:?}");
        assert_eq!(snapshots[0][1], 300.0);
        assert_eq!(clock.running(), Some(Color::Black));
        // Black takes 5 seconds.
        let snapshots = clock.moved(&[Color::Black], Color::White, t0 + secs(17.0));
        assert!((snapshots[0][1] - 298.0).abs() < 1e-6, "300 - 5 + 3: {snapshots:?}");
        assert_eq!(clock.running(), Some(Color::White));
    }

    #[test]
    fn a_move_seen_before_any_press_starts_the_clock_and_costs_nothing() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(300.0, 2.0), t0);
        let snapshots = clock.moved(&[Color::White], Color::Black, t0 + secs(40.0));
        assert_eq!(snapshots[0], [302.0, 300.0], "the first move took no time, and earned the extra time");
        assert!(clock.started());
        assert_eq!(clock.running(), Some(Color::Black));
    }

    #[test]
    fn a_press_before_the_move_is_seen_and_the_move_after_it_agree() {
        let t0 = Instant::now();
        let mut pressed = Clock::new(control(300.0, 3.0), t0);
        pressed.press(Color::White, t0);
        pressed.press(Color::White, t0 + secs(10.0));
        // The camera sees White's move a moment later: the time of Black is running already, the move changes nothing
        // but the extra time of White.
        let after = pressed.moved(&[Color::White], Color::Black, t0 + secs(10.5));
        assert!((after[0][0] - 293.0).abs() < 1e-6, "{after:?}");
        assert_eq!(pressed.running(), Some(Color::Black));
        // Black was running since the press: 0.5 s so far.
        assert!((pressed.left(t0 + secs(10.5))[1] - 299.5).abs() < 1e-6);
    }

    #[test]
    fn a_press_by_mistake_is_mended_by_the_next_move() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(300.0, 0.0), t0);
        clock.press(Color::White, t0);
        // Pressed by mistake after 2 seconds: Black's time runs although White is to move.
        clock.press(Color::White, t0 + secs(2.0));
        assert_eq!(clock.running(), Some(Color::Black));
        // White moves at 6 seconds: the clock is put right, White to move is not the one running, Black to move is.
        clock.moved(&[Color::White], Color::Black, t0 + secs(6.0));
        assert_eq!(clock.running(), Some(Color::Black));
        // Another press, then a move of Black: the turn is White's again.
        clock.press(Color::Black, t0 + secs(8.0));
        assert_eq!(clock.running(), Some(Color::White));
        clock.moved(&[Color::Black], Color::White, t0 + secs(9.0));
        assert_eq!(clock.running(), Some(Color::White));
    }

    #[test]
    fn two_moves_seen_together_both_earn_the_extra_time() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(60.0, 5.0), t0);
        clock.press(Color::White, t0);
        let snapshots = clock.moved(&[Color::White, Color::Black], Color::White, t0 + secs(4.0));
        assert_eq!(snapshots.len(), 2);
        assert!((snapshots[0][0] - 61.0).abs() < 1e-6, "60 - 4 + 5: {snapshots:?}");
        assert!((snapshots[1][1] - 65.0).abs() < 1e-6, "60 + 5: {snapshots:?}");
        assert_eq!(clock.running(), Some(Color::White));
    }

    #[test]
    fn a_pause_stops_both_times_and_a_move_seen_during_it_changes_nothing() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(300.0, 3.0), t0);
        clock.press(Color::White, t0);
        assert!(clock.pause(t0 + secs(10.0)));
        assert!(!clock.pause(t0 + secs(11.0)), "already paused");
        assert_eq!(clock.running(), None);
        assert_eq!(clock.left(t0 + secs(500.0)), [290.0, 300.0]);
        let snapshots = clock.moved(&[Color::White], Color::Black, t0 + secs(20.0));
        assert_eq!(snapshots, vec![[290.0, 300.0]], "no extra time while paused");
        assert!(clock.paused() && clock.running().is_none());
        // Going on gives the time to the player to move, from the moment of the press.
        assert!(clock.resume(Color::Black, t0 + secs(100.0)));
        assert!((clock.left(t0 + secs(104.0))[1] - 296.0).abs() < 1e-6);
        assert!(!clock.resume(Color::Black, t0 + secs(105.0)), "not paused any more");
    }

    #[test]
    fn a_clock_that_has_not_started_cannot_be_paused_and_the_press_goes_on_after_a_pause() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(300.0, 0.0), t0);
        assert!(!clock.pause(t0));
        clock.press(Color::White, t0);
        clock.pause(t0 + secs(5.0));
        clock.press(Color::White, t0 + secs(30.0));
        assert!(!clock.paused());
        assert_eq!(clock.running(), Some(Color::White));
    }

    #[test]
    fn the_time_running_out_stops_the_clock_and_names_the_player() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(10.0, 0.0), t0);
        clock.press(Color::White, t0);
        clock.press(Color::White, t0 + secs(2.0));
        assert_eq!(clock.flagged(t0 + secs(11.0)), None, "Black has had 9 seconds");
        assert_eq!(clock.flagged(t0 + secs(12.5)), Some(Color::Black));
        assert_eq!(clock.expire(t0 + secs(11.0)), None);
        assert_eq!(clock.expire(t0 + secs(12.5)), Some(Color::Black));
        assert_eq!(clock.running(), None);
        assert_eq!(clock.left(t0 + secs(99.0)), [8.0, 0.0]);
    }

    #[test]
    fn an_event_told_late_about_an_earlier_moment_does_not_take_the_clock_back() {
        let t0 = Instant::now();
        let mut clock = Clock::new(control(100.0, 0.0), t0);
        clock.press(Color::White, t0);
        clock.press(Color::White, t0 + secs(10.0));
        // The move was seen at 9 seconds (the board was still) but is only handled after the press at 10.
        clock.moved(&[Color::White], Color::Black, t0 + secs(9.0));
        let left = clock.left(t0 + secs(10.0));
        assert!((left[0] - 90.0).abs() < 1e-6 && (left[1] - 100.0).abs() < 1e-6, "{left:?}");
    }

    #[test]
    fn the_time_control_is_checked() {
        assert!(TimeControl::checked(0.5, 0.0).is_err());
        assert!(TimeControl::checked(f64::NAN, 0.0).is_err());
        assert!(TimeControl::checked(40_000.0, 0.0).is_err());
        assert!(TimeControl::checked(300.0, -1.0).is_err());
        assert!(TimeControl::checked(300.0, 4_000.0).is_err());
        assert_eq!(TimeControl::checked(300.0, 3.0).unwrap().pgn(), "300+3");
        assert_eq!(TimeControl::checked(600.0, 0.0).unwrap().pgn(), "600");
    }

    #[test]
    fn the_clock_of_a_game_comes_from_what_its_moves_say() {
        let t0 = Instant::now();
        let mut game = GameRecord::new("t".into(), None, "A", "B", None).unwrap();
        game.time_control = Some(control(300.0, 3.0));
        let fresh = Clock::from_game(&game, t0, false, true).unwrap();
        assert!(!fresh.started() && fresh.running().is_none(), "nobody moved yet");
        assert_eq!(fresh.left(t0), [300.0, 300.0]);

        game.push_text("e4", 0.0).unwrap();
        game.moves[0].left = Some([303.0, 300.0]);
        let live = Clock::from_game(&game, t0, false, true).unwrap();
        assert_eq!(live.running(), Some(Color::Black));
        assert_eq!(live.left(t0), [303.0, 300.0]);
        // Shown only: no time runs.
        assert_eq!(Clock::from_game(&game, t0, false, false).unwrap().running(), None);
        // A game that is over does not run either.
        game.finish("1-0", "resignation");
        assert_eq!(Clock::from_game(&game, t0, false, true).unwrap().running(), None);
        // A game without a clock has none.
        game.time_control = None;
        assert!(Clock::from_game(&game, t0, false, true).is_none());
    }
}
