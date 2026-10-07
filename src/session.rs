use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub id: u64,
    pub name: String,
    pub directory: String,
    pub command: String,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionsFile {
    sessions: Vec<Session>,
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
        });
    base.join("jetty").join("sessions.json")
}

/// Returns a session with id 0; the engine assigns a real id on Add.
pub fn default_session() -> Session {
    Session {
        id: 0,
        name: "shell".into(),
        directory: std::env::var("HOME").unwrap_or_else(|_| "/".into()),
        command: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
    }
}

pub fn load(path: &Path) -> Vec<Session> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return seed(path);
    };
    match serde_json::from_str::<SessionsFile>(&raw) {
        Ok(file) => {
            let mut sessions = file.sessions;
            if sessions.iter().any(|s| s.id == 0) {
                for (i, s) in sessions.iter_mut().enumerate() {
                    s.id = i as u64 + 1;
                }
                let _ = save(path, &sessions);
            }
            sessions
        }
        Err(e) => {
            let backup = path.with_extension("json.bak");
            eprintln!(
                "jetty: {} is not valid JSON ({e}), moved to {}",
                path.display(),
                backup.display()
            );
            let _ = std::fs::rename(path, &backup);
            seed(path)
        }
    }
}

pub fn save(path: &Path, sessions: &[Session]) -> Result<(), String> {
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        let msg = format!("cannot create {}: {e}", dir.display());
        eprintln!("jetty: {msg}");
        return Err(msg);
    }
    let file = SessionsFile {
        sessions: sessions.to_vec(),
    };
    match serde_json::to_string_pretty(&file) {
        Ok(json) => {
            let tmp = path.with_extension("json.tmp");
            if let Err(e) = std::fs::write(&tmp, &json) {
                let msg = format!("cannot write {}: {e}", tmp.display());
                eprintln!("jetty: {msg}");
                return Err(msg);
            }
            if let Err(e) = std::fs::File::open(&tmp).and_then(|f| f.sync_all()) {
                let msg = format!("cannot fsync {}: {e}", tmp.display());
                eprintln!("jetty: {msg}");
                let _ = std::fs::remove_file(&tmp);
                return Err(msg);
            }
            if let Err(e) = std::fs::rename(&tmp, path) {
                let msg = format!("cannot rename {} to {}: {e}", tmp.display(), path.display());
                eprintln!("jetty: {msg}");
                let _ = std::fs::remove_file(&tmp);
                return Err(msg);
            }
            Ok(())
        }
        Err(e) => {
            let msg = format!("cannot serialize sessions: {e}");
            eprintln!("jetty: {msg}");
            Err(msg)
        }
    }
}

fn seed(path: &Path) -> Vec<Session> {
    let mut session = default_session();
    session.id = 1;
    let sessions = vec![session];
    if let Err(e) = save(path, &sessions) {
        eprintln!("jetty: failed to seed sessions file: {e}");
        return vec![];
    }
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetty-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("sessions.json")
    }

    #[test]
    fn seed_round_trip_and_corrupt_recovery() {
        let path = tmp_path("seed_rt");
        let expected = vec![Session {
            id: 1,
            ..default_session()
        }];
        assert_eq!(load(&path), expected);
        assert!(path.exists());

        let mut sessions = load(&path);
        sessions[0].name = "agent".into();
        save(&path, &sessions).unwrap();
        assert_eq!(load(&path), sessions);

        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), expected);
        assert!(path.with_extension("json.bak").exists());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_ids_migration() {
        let path = tmp_path("missing_ids");
        // Raw JSON without id fields — pre-3b-ids format.
        let old_json = r#"{
  "sessions": [
    {
      "name": "project",
      "directory": "/home/user/project",
      "command": "/bin/zsh"
    },
    {
      "name": "agent",
      "directory": "/tmp/agent",
      "command": "/bin/bash"
    }
  ]
}"#;
        std::fs::write(&path, old_json).unwrap();

        let sessions = load(&path);

        // Both sessions now have non-zero sequential ids.
        assert_eq!(sessions.len(), 2, "should load both sessions");
        assert_eq!(sessions[0].id, 1, "first session id should be 1");
        assert_eq!(sessions[1].id, 2, "second session id should be 2");

        // Fields are preserved.
        assert_eq!(sessions[0].name, "project");
        assert_eq!(sessions[0].directory, "/home/user/project");
        assert_eq!(sessions[0].command, "/bin/zsh");
        assert_eq!(sessions[1].name, "agent");
        assert_eq!(sessions[1].directory, "/tmp/agent");
        assert_eq!(sessions[1].command, "/bin/bash");

        // On-disk file was updated with id fields.
        let raw = std::fs::read_to_string(&path).unwrap();
        let file: SessionsFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(file.sessions.len(), 2);
        assert_eq!(file.sessions[0].id, 1);
        assert_eq!(file.sessions[1].id, 2);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn mixed_ids_migration() {
        let path = tmp_path("mixed_ids");
        // One session with id: 42 (non-zero), one with id: 0, one missing id.
        let old_json = r#"{
  "sessions": [
    {
      "id": 42,
      "name": "legacy",
      "directory": "/home/x",
      "command": "/bin/fish"
    },
    {
      "id": 0,
      "name": "zero-id",
      "directory": "/tmp/z",
      "command": "/bin/sh"
    },
    {
      "name": "missing-id",
      "directory": "/var/run",
      "command": "/bin/dash"
    }
  ]
}"#;
        std::fs::write(&path, old_json).unwrap();

        let sessions = load(&path);

        // All sessions are renumbered 1..n regardless of original id.
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].id, 1, "even id 42 gets renumbered to 1");
        assert_eq!(sessions[1].id, 2);
        assert_eq!(sessions[2].id, 3);

        // Names and fields are preserved.
        assert_eq!(sessions[0].name, "legacy");
        assert_eq!(sessions[1].name, "zero-id");
        assert_eq!(sessions[2].name, "missing-id");

        // On-disk file was updated with sequential ids.
        let raw = std::fs::read_to_string(&path).unwrap();
        let file: SessionsFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(file.sessions.len(), 3);
        assert_eq!(file.sessions[0].id, 1);
        assert_eq!(file.sessions[1].id, 2);
        assert_eq!(file.sessions[2].id, 3);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reload_idempotent_after_migration() {
        let path = tmp_path("reload_idempotent");
        // Pre-migration JSON with mixed id states.
        let old_json = r#"{
  "sessions": [
    {
      "id": 42,
      "name": "legacy",
      "directory": "/home/x",
      "command": "/bin/fish"
    },
    {
      "id": 0,
      "name": "zero-id",
      "directory": "/tmp/z",
      "command": "/bin/sh"
    },
    {
      "name": "missing-id",
      "directory": "/var/run",
      "command": "/bin/dash"
    }
  ]
}"#;
        std::fs::write(&path, old_json).unwrap();

        // First load triggers migration — renumbers to 1, 2, 3.
        let first = load(&path);
        assert_eq!(first.len(), 3);
        assert_eq!(first[0].id, 1);
        assert_eq!(first[1].id, 2);
        assert_eq!(first[2].id, 3);

        // Second load on the already-migrated file: ids must be identical.
        let second = load(&path);
        assert_eq!(second, first, "ids must not change across reloads");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn single_session_id_zero_migration() {
        let path = tmp_path("single_id_zero");
        let old_json = r#"{
  "sessions": [
    {
      "id": 0,
      "name": "single-zero",
      "directory": "/home/zero",
      "command": "/bin/zsh"
    }
  ]
}"#;
        std::fs::write(&path, old_json).unwrap();

        let sessions = load(&path);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].id, 1,
            "single session with id:0 renumbered to 1"
        );
        assert_eq!(sessions[0].name, "single-zero");
        assert_eq!(sessions[0].directory, "/home/zero");
        assert_eq!(sessions[0].command, "/bin/zsh");

        // On-disk file was updated with id 1.
        let raw = std::fs::read_to_string(&path).unwrap();
        let file: SessionsFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(file.sessions.len(), 1);
        assert_eq!(file.sessions[0].id, 1);

        // Second load preserves the migrated id.
        let reloaded = load(&path);
        assert_eq!(reloaded, sessions, "id must not change on reload");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn seed_unwritable_path_returns_empty_without_panic() {
        // A regular file where the config dir should be makes create_dir_all fail.
        let dir =
            std::env::temp_dir().join(format!("jetty-seed-unwritable-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        std::fs::write(&dir, "block").unwrap();
        let path = dir.join("sessions.json");

        let sessions = seed(&path);
        assert!(
            sessions.is_empty(),
            "seed on unwritable path must return empty vec"
        );

        let _ = std::fs::remove_file(&dir);
    }
}
