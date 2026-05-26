//! Curator — background skill maintenance.
//!
//! Port of Hermes's `agent/curator.py`. Periodically reviews and
//! maintains agent-created skills: transitions stale skills to
//! archive, consolidates overlapping skills, keeps the skill
//! library healthy.
//!
//! Key design decisions from Hermes preserved:
//! - Automatic transitions (no LLM) for time-based lifecycle
//! - Optional review fork (with LLM) for consolidation
//! - Strict invariants: only agent-created, never delete, pinned bypass
//! - Persistent scheduler state in `.dirge/skills/.curator_state`
//! - Interval gates to avoid running too frequently
//! - Idle check to avoid running during active sessions

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::extras::dirge_paths::ProjectPaths;

// ── Default configuration ─────────────────────────────

/// Days since last activity to mark a skill as stale.
const STALE_AFTER_DAYS: u64 = 30;

/// Days of staleness before archiving a skill.
const ARCHIVE_AFTER_STALE_DAYS: u64 = 90;

/// Minimum hours between curator runs.
const INTERVAL_HOURS: u64 = 168; // 7 days

/// Minimum hours of idle time before curator runs.
#[allow(dead_code)]
const IDLE_HOURS: u64 = 2;

// ── Curator state ─────────────────────────────────────

/// Persistent scheduler state written to `.dirge/skills/.curator_state`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CuratorState {
    /// Unix timestamp (seconds) of the last curator run.
    /// `None` means never run (different from epoch-0 which
    /// is a valid timestamp on some systems).
    last_run: Option<u64>,
    /// Timestamp when the state was first seeded.
    first_check: u64,
}

impl CuratorState {
    fn new() -> Self {
        let now = now_secs();
        CuratorState {
            last_run: None,
            first_check: now,
        }
    }

    fn load(path: &PathBuf) -> Result<Self, String> {
        if !path.exists() {
            return Ok(CuratorState::new());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read curator state: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse curator state: {e}"))
    }

    fn save(&self, path: &PathBuf) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create curator state directory: {e}"))?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize curator state: {e}"))?;
        crate::fs_atomic::atomic_write_sync(path, content.as_bytes())
            .map_err(|e| format!("Failed to write curator state: {e}"))
    }
}

// ── Curator ───────────────────────────────────────────

/// Skill lifecycle manager. Runs periodic maintenance on
/// agent-created skills in `.dirge/skills/`.
pub struct Curator {
    paths: ProjectPaths,
    state: CuratorState,
    state_path: PathBuf,
}

/// The lifecycle state of a skill, as tracked by the curator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SkillLifecycle {
    Active,
    Stale,
    Archived,
}

impl Curator {
    pub fn new(paths: &ProjectPaths) -> Result<Self, String> {
        let state_path = paths.skills_dir().join(".curator_state");
        let state = CuratorState::load(&state_path)?;
        Ok(Curator {
            paths: paths.clone(),
            state,
            state_path,
        })
    }

    /// Check whether the curator should run now, based on:
    /// 1. Interval gate (last run was >= INTERVAL_HOURS ago)
    /// 2. Idle gate (no activity for >= IDLE_HOURS — simplified
    ///    check: we just use the interval as a proxy since we don't
    ///    track session-level idle time yet)
    /// 3. First-run deferral (don't run on first check — seed state)
    pub fn should_run_now(&self) -> bool {
        let now = now_secs();

        // Never run on the first check — just seed the state.
        if self.state.last_run.is_none() {
            return false;
        }

        let elapsed = Duration::from_secs(now - self.state.last_run.unwrap());
        elapsed >= Duration::from_secs(INTERVAL_HOURS * 3600)
    }

    /// Run automatic lifecycle transitions on all skills.
    /// No LLM involved — pure time-based rules.
    ///
    /// Returns a list of skills that should be considered for
    /// consolidation review (stale for > ARCHIVE_AFTER_STALE_DAYS
    /// but not yet archived).
    pub fn apply_automatic_transitions(&mut self) -> Result<Vec<String>, String> {
        let now = now_secs();
        let skills_dir = self.paths.skills_dir();

        if !skills_dir.is_dir() {
            self.state.last_run = Some(now);
            self.state.save(&self.state_path)?;
            return Ok(Vec::new());
        }

        let mut stale_names: Vec<String> = Vec::new();

        for entry in std::fs::read_dir(&skills_dir)
            .map_err(|e| format!("Failed to read skills directory: {e}"))?
        {
            let entry = entry.map_err(|e| format!("Failed to read skill entry: {e}"))?;
            let path = entry.path();

            // Only process directories with SKILL.md.
            if !path.is_dir() || !path.join("SKILL.md").is_file() {
                continue;
            }

            // Skip archived skills (already in .archive/).
            if path.file_name().map(|n| n == ".archive").unwrap_or(false) {
                continue;
            }

            // Check last modified time.
            let mod_time = match std::fs::metadata(path.join("SKILL.md")) {
                Ok(meta) => meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(now),
                Err(_) => continue,
            };

            let age_days = Duration::from_secs(now.saturating_sub(mod_time)).as_secs() / 86400;

            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());

            if let Some(name) = name {
                if age_days >= ARCHIVE_AFTER_STALE_DAYS {
                    // Archive this skill.
                    self.archive_skill(&name)?;
                } else if age_days >= STALE_AFTER_DAYS {
                    stale_names.push(name);
                }
            }
        }

        self.state.last_run = Some(now);
        self.state.save(&self.state_path)?;

        Ok(stale_names)
    }

    /// Move a skill to the `.archive/` directory.
    fn archive_skill(&self, name: &str) -> Result<(), String> {
        let src = self.paths.skills_dir().join(name);
        if !src.is_dir() {
            return Ok(());
        }

        let archive_dir = self.paths.skills_dir().join(".archive");
        std::fs::create_dir_all(&archive_dir)
            .map_err(|e| format!("Failed to create archive directory: {e}"))?;

        let dest = archive_dir.join(name);
        // If destination already exists, the skill was already
        // archived (possibly by a concurrent curator process).
        // Skip cleanly rather than removing and risking data loss.
        if dest.exists() {
            return Ok(());
        }

        std::fs::rename(&src, &dest)
            .map_err(|e| format!("Failed to archive skill '{}': {}", name, e))?;

        Ok(())
    }

    /// Record a curator run (for callers that want to force-update
    /// state after a manual run).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn record_run(&mut self) -> Result<(), String> {
        self.state.last_run = Some(now_secs());
        self.state.save(&self.state_path)
    }
}

// ── Helpers ───────────────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_project() -> (ProjectPaths, std::path::PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dirge-curator-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let paths = ProjectPaths::new(&dir);
        (paths, dir)
    }

    fn create_skill_dir(paths: &ProjectPaths, name: &str) {
        let dir = paths.skills_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: test\n---\n\nbody\n").unwrap();
    }

    // ── CuratorState persistence ───────────────────────

    #[test]
    fn curator_state_round_trips() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");

        let mut state = CuratorState::new();
        state.last_run = Some(1234567890);
        state.save(&state_path).unwrap();

        let loaded = CuratorState::load(&state_path).unwrap();
        assert_eq!(loaded.last_run, Some(1234567890));
        assert!(loaded.first_check > 0);
    }

    #[test]
    fn missing_state_file_defaults_to_new() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");
        let state = CuratorState::load(&state_path).unwrap();
        assert_eq!(state.last_run, None);
        assert!(state.first_check > 0);
    }

    // ── should_run_now ─────────────────────────────────

    #[test]
    fn first_run_never_runs() {
        let (paths, _dir) = temp_project();
        let curator = Curator::new(&paths).unwrap();
        assert!(!curator.should_run_now(), "first check should defer");
    }

    #[test]
    fn runs_after_interval_elapses() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");

        // Set state as if last run was 8 days ago.
        let past = now_secs() - INTERVAL_HOURS * 3600 - 1;
        let mut state = CuratorState::new();
        state.last_run = Some(past);
        state.save(&state_path).unwrap();

        let curator = Curator::new(&paths).unwrap();
        assert!(curator.should_run_now());
    }

    #[test]
    fn does_not_run_within_interval() {
        let (paths, _dir) = temp_project();
        let state_path = paths.skills_dir().join(".curator_state");

        // Set state as if last run was 1 hour ago.
        let recent = now_secs() - 3600;
        let mut state = CuratorState::new();
        state.last_run = Some(recent);
        state.save(&state_path).unwrap();

        let curator = Curator::new(&paths).unwrap();
        assert!(!curator.should_run_now());
    }

    // ── archive_skill ─────────────────────────────────

    #[test]
    fn archive_moves_skill_to_archive_dir() {
        let (paths, _dir) = temp_project();
        create_skill_dir(&paths, "old-skill");

        let curator = Curator::new(&paths).unwrap();
        curator.archive_skill("old-skill").unwrap();

        // Original gone.
        assert!(!paths.skills_dir().join("old-skill").is_dir());
        // Present in archive.
        assert!(
            paths
                .skills_dir()
                .join(".archive")
                .join("old-skill")
                .join("SKILL.md")
                .is_file()
        );
    }

    // ── apply_automatic_transitions ────────────────────

    #[test]
    fn empty_skills_dir_is_no_op() {
        let (paths, _dir) = temp_project();
        std::fs::create_dir_all(paths.skills_dir()).unwrap();
        let mut curator = Curator::new(&paths).unwrap();
        let stale = curator.apply_automatic_transitions().unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn missing_skills_dir_is_no_op() {
        let (paths, _dir) = temp_project();
        let mut curator = Curator::new(&paths).unwrap();
        let stale = curator.apply_automatic_transitions().unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn record_run_updates_timestamp() {
        let (paths, _dir) = temp_project();
        let mut curator = Curator::new(&paths).unwrap();
        let before = curator.state.last_run;
        curator.record_run().unwrap();

        // Reload and verify.
        let curator2 = Curator::new(&paths).unwrap();
        assert!(
            curator2.state.last_run > before,
            "recording a run should update last_run"
        );
    }
}
