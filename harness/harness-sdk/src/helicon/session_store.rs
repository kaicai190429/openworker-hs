//! Per-session adapter state that has to survive a restart: where archiving and extraction stand in the
//! engine's history, and whether the session's opening bridge was already shown. Plain JSON files under
//! `<state_dir>/helicon-memory/sessions/`, written atomically.
use super::{
    log::Logger,
    util::{iso_now, now_ms},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRecord {
    /// ISO time the session was first seen: HeliconSearch's `session_datetime`.
    #[serde(default = "iso_now")]
    pub started_at: String,
    /// A new session whose first turn still gets the cross-session bridge.
    #[serde(default)]
    pub bridge_pending: bool,
    /// Next history index to archive (`turns.v1`).
    #[serde(default)]
    pub archive_next: usize,
    /// Next history index to hand to extraction.
    #[serde(default)]
    pub extract_next: usize,
    /// Ms epoch of the session's last turn (or the start of one): what idle ending goes by.
    #[serde(default)]
    pub last_active: u64,
    /// HeliconSearch was told the session is over; a later turn clears it.
    #[serde(default)]
    pub ended: bool,
    /// When `last_active` was last written to the file (not itself saved): activity is noted in memory at
    /// once, and written at most every few seconds.
    #[serde(skip)]
    pub saved_at: u64,
}

impl SessionRecord {
    fn fresh() -> Self {
        Self {
            started_at: iso_now(),
            bridge_pending: false,
            archive_next: 0,
            extract_next: 0,
            last_active: now_ms(),
            ended: false,
            saved_at: 0,
        }
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

pub(crate) struct SessionStore {
    dir: PathBuf,
    cache: HashMap<String, SessionRecord>,
    logger: Logger,
    /// The last save failed (said once per run of failures, not on every save).
    failing: bool,
}

impl SessionStore {
    pub(crate) fn new(root: PathBuf, logger: Logger) -> Self {
        Self {
            dir: root.join("sessions"),
            cache: HashMap::new(),
            logger,
            failing: false,
        }
    }

    fn file(&self, session_id: &str) -> Option<PathBuf> {
        safe_id(session_id).then(|| self.dir.join(format!("{session_id}.json")))
    }

    /// Whether the adapter has seen this session before (in this process or a previous one).
    pub(crate) fn has(&self, session_id: &str) -> bool {
        self.cache.contains_key(session_id) || self.file(session_id).is_some_and(|f| f.is_file())
    }

    /// The record, or a fresh one (not saved until `save`).
    pub(crate) fn get(&mut self, session_id: &str) -> &mut SessionRecord {
        if !self.cache.contains_key(session_id) {
            let file = self.file(session_id).filter(|f| f.is_file());
            let loaded = file.as_ref().and_then(|f| {
                // a file that cannot be read or understood starts the session's bookkeeping over: the
                // server de-duplicates a resend. That is said, since everything of it is sent again.
                let record = std::fs::read_to_string(f)
                    .map_err(|e| e.to_string())
                    .and_then(|text| {
                        serde_json::from_str::<SessionRecord>(&text).map_err(|e| e.to_string())
                    });
                match record {
                    Ok(r) => Some(r),
                    Err(why) => {
                        self.logger.warn(&format!(
                            "[helicon-memory] the saved progress of session {session_id} could not be used ({why}); it starts over and is sent again"
                        ));
                        None
                    }
                }
            });
            self.cache.insert(
                session_id.to_owned(),
                loaded.unwrap_or_else(SessionRecord::fresh),
            );
        }
        self.cache.get_mut(session_id).expect("just inserted")
    }

    pub(crate) fn save(&mut self, session_id: &str) {
        let Some(rec) = self.cache.get(session_id) else {
            return;
        };
        let Some(file) = self.file(session_id) else {
            return;
        };
        // The cursors are an optimisation: without them the next run resends and the server de-duplicates.
        // A failure is still said, once, so a full disk or a bad permission does not go unnoticed.
        let saved = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            static SAVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let tmp = file.with_extension(format!(
                "{}.{}.tmp",
                std::process::id(),
                SAVES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let text = serde_json::to_string(rec).map_err(std::io::Error::other)?;
            write_private(&tmp, text.as_bytes())?;
            std::fs::rename(&tmp, &file)
        })();
        match saved {
            Ok(()) => self.failing = false,
            Err(err) if !self.failing => {
                self.failing = true;
                self.logger.warn(&format!(
                    "[helicon-memory] could not save the progress of session {session_id}: {err} (it will be sent again after a restart)"
                ));
            }
            Err(_) => {}
        }
    }

    /// Ids of the saved sessions (those of earlier runs too).
    pub(crate) fn list(&self) -> Vec<String> {
        std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name.strip_suffix(".json").map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drops the in-memory copy (the file stays: a later resume continues from it).
    pub(crate) fn forget(&mut self, session_id: &str) {
        self.cache.remove(session_id);
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_survives_a_restart_and_a_damaged_file_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = SessionStore::new(
            dir.path().to_path_buf(),
            Logger::new(dir.path().join("log"), false),
        );
        assert!(!a.has("s1"));
        {
            let rec = a.get("s1");
            rec.archive_next = 7;
            rec.bridge_pending = true;
        }
        a.save("s1");
        assert!(a.has("s1"));
        let mut b = SessionStore::new(
            dir.path().to_path_buf(),
            Logger::new(dir.path().join("log"), false),
        );
        assert!(b.has("s1"));
        assert_eq!(b.get("s1").archive_next, 7);
        assert!(b.get("s1").bridge_pending);
        assert_eq!(b.list(), vec!["s1".to_owned()]);
        std::fs::write(dir.path().join("sessions").join("s2.json"), "{broken").unwrap();
        let mut c = SessionStore::new(
            dir.path().to_path_buf(),
            Logger::new(dir.path().join("log"), false),
        );
        assert_eq!(c.get("s2").archive_next, 0);
    }

    #[test]
    fn an_unsafe_id_is_never_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SessionStore::new(
            dir.path().to_path_buf(),
            Logger::new(dir.path().join("log"), false),
        );
        s.get("../x").archive_next = 1;
        s.save("../x");
        assert!(!dir.path().join("x.json").exists());
        assert!(s.list().is_empty());
    }
}
