use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::config;

/// Estado persistido por tarefa, sobrevive a reinícios do daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskState {
    #[serde(default)]
    pub last_fired: Option<DateTime<Local>>,
    #[serde(default)]
    pub last_check: Option<DateTime<Local>>,
    /// Saída da última checagem (usada por `when = "output_changed"`).
    #[serde(default)]
    pub last_output: Option<String>,
}

static LOCK: Mutex<()> = Mutex::new(());

fn path(name: &str) -> PathBuf {
    config::state_dir().join(format!("{name}.json"))
}

pub fn load(name: &str) -> TaskState {
    std::fs::read_to_string(path(name))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Leitura-modificação-escrita serializada dentro do processo.
pub fn update(name: &str, f: impl FnOnce(&mut TaskState)) -> Result<TaskState> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = load(name);
    f(&mut st);
    std::fs::create_dir_all(config::state_dir())?;
    std::fs::write(path(name), serde_json::to_vec_pretty(&st)?)?;
    Ok(st)
}

pub fn remove(name: &str) {
    let _ = std::fs::remove_file(path(name));
}
