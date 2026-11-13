//! "Bot is running" marker in the data dir (`antbot.lock`).
//!
//! `antbot run` creates it and refreshes its heartbeat while it runs;
//! `antbot reset-kill` refuses to touch the risk state while the marker is
//! fresh, because the running bot keeps its kill in memory and would write
//! it back (silently undoing the reset). A marker whose heartbeat is older
//! than [`LOCK_STALE_MS`] is left over from a crash and is ignored.
//! A plain heartbeat file (no OS lock) also works across Docker restarts,
//! where PIDs are reused.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Heartbeat age after which the marker is considered stale.
pub const LOCK_STALE_MS: u64 = 120_000;
/// How often the running bot refreshes the heartbeat.
pub const LOCK_REFRESH_MS: u64 = 20_000;

pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("antbot.lock")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub mode: String,
    pub started_ms: u64,
    pub heartbeat_ms: u64,
}

impl LockInfo {
    pub fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.heartbeat_ms) < LOCK_STALE_MS
    }
}

/// Read the marker, if any (an unreadable marker counts as absent).
pub fn read(data_dir: &Path) -> Option<LockInfo> {
    let text = std::fs::read_to_string(lock_path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// A running bot that holds the data dir, if any.
pub fn held(data_dir: &Path, now_ms: u64) -> Option<LockInfo> {
    read(data_dir).filter(|l| l.is_fresh(now_ms))
}

/// The marker held by this process.
pub struct RunLock {
    path: PathBuf,
    info: LockInfo,
}

impl RunLock {
    /// Take the marker; fails if another fresh one exists.
    pub fn acquire(data_dir: &Path, mode: &str, now_ms: u64) -> anyhow::Result<Self> {
        // Same PID as ours: a leftover of a previous run in this PID
        // namespace (Docker restarts reuse PID 1), not a live instance.
        let me = std::process::id();
        if let Some(other) = held(data_dir, now_ms).filter(|l| l.pid != me) {
            anyhow::bail!(
                "data dir {} is in use by another antbot (pid {}, mode {}, heartbeat {} s ago); \
                 stop it first (or delete {} if it is really gone)",
                data_dir.display(),
                other.pid,
                other.mode,
                now_ms.saturating_sub(other.heartbeat_ms) / 1000,
                lock_path(data_dir).display()
            );
        }
        let mut l = Self {
            path: lock_path(data_dir),
            info: LockInfo {
                pid: me,
                mode: mode.to_string(),
                started_ms: now_ms,
                heartbeat_ms: now_ms,
            },
        };
        l.refresh(now_ms)?;
        Ok(l)
    }

    /// Update the heartbeat (atomic write).
    pub fn refresh(&mut self, now_ms: u64) -> anyhow::Result<()> {
        self.info.heartbeat_ms = now_ms;
        let tmp = self.path.with_extension("lock.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&self.info)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

impl Drop for RunLock {
    /// Remove the marker on any exit path that unwinds.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_other(d: &Path, pid: u32, heartbeat_ms: u64) {
        let info = LockInfo {
            pid,
            mode: "paper".into(),
            started_ms: heartbeat_ms,
            heartbeat_ms,
        };
        std::fs::write(lock_path(d), serde_json::to_vec(&info).unwrap()).unwrap();
    }

    #[test]
    fn acquire_refuse_stale_release() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert!(held(d, 1_000).is_none());
        let other = std::process::id().wrapping_add(1);
        write_other(d, other, 1_000);
        assert_eq!(held(d, 2_000).unwrap().pid, other);
        // Another instance is refused while its heartbeat is fresh.
        assert!(RunLock::acquire(d, "paper", 2_000).is_err());
        // Stale after LOCK_STALE_MS without refresh (crash leftover).
        assert!(held(d, 1_000 + LOCK_STALE_MS).is_none());
        let l = RunLock::acquire(d, "paper", 1_000 + LOCK_STALE_MS).unwrap();
        assert_eq!(
            held(d, 2_000 + LOCK_STALE_MS).unwrap().pid,
            std::process::id()
        );
        drop(l);
        assert!(read(d).is_none());
        // A fresh marker with our own PID is a leftover (PID reuse).
        write_other(d, std::process::id(), 5_000);
        let l = RunLock::acquire(d, "paper", 5_001).unwrap();
        drop(l);
    }
}
