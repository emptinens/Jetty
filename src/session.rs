use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
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

pub fn default_session() -> Session {
    Session {
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
        Ok(file) => file.sessions,
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

pub fn save(path: &Path, sessions: &[Session]) {
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("jetty: cannot create {}: {e}", dir.display());
            return;
        }
    }
    let file = SessionsFile {
        sessions: sessions.to_vec(),
    };
    match serde_json::to_string_pretty(&file) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                eprintln!("jetty: cannot write {}: {e}", path.display());
            }
        }
        Err(e) => eprintln!("jetty: cannot serialize sessions: {e}"),
    }
}

fn seed(path: &Path) -> Vec<Session> {
    let sessions = vec![default_session()];
    save(path, &sessions);
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetty-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("sessions.json")
    }

    #[test]
    fn seed_round_trip_and_corrupt_recovery() {
        let path = tmp_path();
        assert_eq!(load(&path), vec![default_session()]);
        assert!(path.exists());

        let mut sessions = load(&path);
        sessions[0].name = "agent".into();
        save(&path, &sessions);
        assert_eq!(load(&path), sessions);

        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), vec![default_session()]);
        assert!(path.with_extension("json.bak").exists());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}