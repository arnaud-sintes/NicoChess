//! Games on disk: one JSON file per game in a data directory.
//!
//! Files are written in one step (temporary file, then rename), so a power loss
//! never leaves a half-written game.

use crate::game::GameRecord;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Store {
    dir: PathBuf,
}

/// What the list of games shows.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Summary {
    pub id: String,
    pub started_at: Option<String>,
    pub white: String,
    pub black: String,
    pub result: Option<String>,
    pub termination: Option<String>,
    pub moves: usize,
}

impl Summary {
    pub fn of(game: &GameRecord) -> Summary {
        Summary {
            id: game.id.clone(),
            started_at: game.started_at.clone(),
            white: game.white.clone(),
            black: game.black.clone(),
            result: game.result.clone(),
            termination: game.termination.clone(),
            moves: game.moves.len(),
        }
    }
}

/// Only letters, digits and dashes: safe to use as a file name.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(Store { dir: dir.to_path_buf() })
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            bail!("invalid game id");
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    /// A new id that starts with the date, so ids sort by date.
    pub fn new_id(&self, started_at: Option<&str>) -> String {
        let stamp: String = match started_at {
            Some(s) => s.chars().filter(|c| c.is_ascii_digit() || *c == 'T').take(15).collect(),
            None => String::new(),
        };
        let base = if stamp.len() == 15 { stamp } else { "undated".to_string() };
        let mut id = base.clone();
        let mut n = 2;
        while self.path(&id).map(|p| p.exists()).unwrap_or(false) {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }

    pub fn save(&self, game: &GameRecord) -> Result<()> {
        let path = self.path(&game.id)?;
        crate::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(game)?)
    }

    pub fn load(&self, id: &str) -> Result<Option<GameRecord>> {
        let path = self.path(id)?;
        match fs::read_to_string(&path) {
            Ok(text) => {
                let game: GameRecord = serde_json::from_str(&text).with_context(|| format!("invalid game file {}", path.display()))?;
                if game.version > crate::game::FORMAT_VERSION {
                    anyhow::bail!("{} was written by a newer version of NicoChess (format {})", path.display(), game.version);
                }
                Ok(Some(game))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Deletes a game (one that was never played, for example). A missing file is not an error.
    pub fn remove(&self, id: &str) -> Result<()> {
        let path = self.path(id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot delete {}", path.display())),
        }
    }

    /// The newest game, if it was never finished (a crash, a power cut or a stop in the middle of a game).
    pub fn latest_unfinished(&self) -> Result<Option<GameRecord>> {
        let Some(newest) = self.list()?.into_iter().next() else { return Ok(None) };
        if newest.result.is_some() {
            return Ok(None);
        }
        self.load(&newest.id)
    }

    /// All games, most recent first. Unreadable files are skipped.
    pub fn list(&self) -> Result<Vec<Summary>> {
        let mut games = Vec::new();
        for entry in fs::read_dir(&self.dir).with_context(|| format!("cannot read {}", self.dir.display()))? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            match self.load(id) {
                Ok(Some(game)) => games.push(Summary::of(&game)),
                Ok(None) => {}
                Err(e) => eprintln!("store: skipping {id}: {e:#}"),
            }
        }
        // Ids start with the date, newest first; undated games ("undated", "undated-2") come last.
        games.sort_by(|a, b| a.id.starts_with("undated").cmp(&b.id.starts_with("undated")).then_with(|| b.id.cmp(&a.id)));
        Ok(games)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undated_games_come_last_and_do_not_count_as_the_newest() {
        let dir = std::env::temp_dir().join(format!("nicochess-order-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir).unwrap();
        let mut first = GameRecord::new("20260101T100000".into(), None, "A", "B", None).unwrap();
        first.finish("1-0", "resignation");
        let mut second = GameRecord::new("20260202T100000".into(), None, "A", "B", None).unwrap();
        second.finish("0-1", "resignation");
        let undated = GameRecord::new("undated".into(), None, "A", "B", None).unwrap(); // never finished
        for game in [&first, &second, &undated] {
            store.save(game).unwrap();
        }
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|g| g.id).collect();
        assert_eq!(ids, ["20260202T100000", "20260101T100000", "undated"]);
        // The newest game is a finished one: nothing to offer, whatever the undated one is.
        assert!(store.latest_unfinished().unwrap().is_none());

        // A newer game that was not finished is the one offered.
        let third = GameRecord::new("20260303T100000".into(), None, "A", "B", None).unwrap();
        store.save(&third).unwrap();
        assert_eq!(store.latest_unfinished().unwrap().unwrap().id, "20260303T100000");

        store.remove("20260303T100000").unwrap();
        store.remove("20260303T100000").unwrap(); // already gone: not an error
        assert!(store.latest_unfinished().unwrap().is_none());
        fs::remove_dir_all(&dir).ok();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nicochess-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    fn game(store: &Store, started: &str, san: &[&str]) -> GameRecord {
        let id = store.new_id(Some(started));
        let mut g = GameRecord::new(id, Some(started.into()), "A", "B", None).unwrap();
        for (i, m) in san.iter().enumerate() {
            g.push_text(m, i as f64 + 1.0).unwrap();
        }
        g
    }

    #[test]
    fn saves_loads_and_lists_most_recent_first() {
        let dir = temp_dir("store");
        let store = Store::open(&dir).unwrap();
        let older = game(&store, "2026-11-01T09:00:00.000Z", &["e4", "e5"]);
        let newer = game(&store, "2026-11-04T10:15:30.000Z", &["d4"]);
        store.save(&older).unwrap();
        store.save(&newer).unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "20261104T101530");
        assert_eq!(list[0].moves, 1);
        assert_eq!(list[1].id, "20261101T090000");

        let loaded = store.load("20261101T090000").unwrap().unwrap();
        assert_eq!(loaded.moves, older.moves);
        assert!(store.load("nope").unwrap().is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ids_do_not_collide_and_are_checked() {
        let dir = temp_dir("ids");
        let store = Store::open(&dir).unwrap();
        let first = game(&store, "2026-11-04T10:15:30.000Z", &[]);
        store.save(&first).unwrap();
        assert_eq!(store.new_id(Some("2026-11-04T10:15:30.000Z")), "20261104T101530-2");
        assert_eq!(store.new_id(None), "undated");

        assert!(store.load("../secret").is_err());
        assert!(store.load("a/b").is_err());
        assert!(!valid_id(""));
        assert!(valid_id("20261104T101530-2"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_broken_file_does_not_hide_the_other_games() {
        let dir = temp_dir("broken");
        let store = Store::open(&dir).unwrap();
        store.save(&game(&store, "2026-11-04T10:15:30.000Z", &["e4"])).unwrap();
        fs::write(dir.join("junk.json"), "{ not json").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        fs::remove_dir_all(&dir).ok();
    }
}
