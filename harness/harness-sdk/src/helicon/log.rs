//! The extension's log: warnings always, the rest only with `debug: true`, appended to
//! `<state_dir>/helicon-memory/adapter.log`. It never writes to stdout or stderr: the host
//! application owns them.
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Logger {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    debug: bool,
    lock: Mutex<()>,
}

impl Logger {
    pub(crate) fn new(path: PathBuf, debug: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                path,
                debug,
                lock: Mutex::new(()),
            }),
        }
    }
    pub(crate) fn info(&self, message: &str) {
        if self.inner.debug {
            self.write("INFO", message);
        }
    }
    pub(crate) fn debug(&self, message: &str) {
        if self.inner.debug {
            self.write("DEBUG", message);
        }
    }
    pub(crate) fn warn(&self, message: &str) {
        self.write("WARN", message);
    }
    fn write(&self, level: &str, message: &str) {
        // logging must never break the host
        let _guard = self.inner.lock.lock();
        let path = &self.inner.path;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
            let mut rotated = path.clone().into_os_string();
            rotated.push(".1");
            let _ = std::fs::rename(path, rotated);
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        // the log can hold fragments of what users said: readable by the owner only
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(path) {
            let _ = writeln!(file, "{} {level} {message}", super::util::iso_now());
        }
    }
}
