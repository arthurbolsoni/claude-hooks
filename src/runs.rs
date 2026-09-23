use anyhow::{Context, Result, bail};
use std::path::PathBuf;

use crate::config;
use crate::runner::RunMeta;

pub fn run_dir(task: &str, run_id: &str) -> PathBuf {
    config::runs_dir().join(task).join(run_id)
}

/// Execuções mais recentes primeiro. `task = None` lista de todas as tarefas.
pub fn list(task: Option<&str>) -> Vec<RunMeta> {
    let root = config::runs_dir();
    let task_dirs: Vec<PathBuf> = match task {
        Some(t) => vec![root.join(t)],
        None => std::fs::read_dir(&root)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for td in task_dirs {
        let Ok(rd) = std::fs::read_dir(&td) else { continue };
        for e in rd.flatten() {
            let meta = e.path().join("meta.json");
            if let Some(m) = std::fs::read_to_string(&meta).ok().and_then(|s| serde_json::from_str::<RunMeta>(&s).ok()) {
                out.push(m);
            }
        }
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    out
}

pub fn find(task: &str, run_id: Option<&str>) -> Result<RunMeta> {
    match run_id {
        Some(id) => {
            let p = run_dir(task, id).join("meta.json");
            let s = std::fs::read_to_string(&p).with_context(|| format!("execução não encontrada: {}", p.display()))?;
            Ok(serde_json::from_str(&s)?)
        }
        None => match list(Some(task)).into_iter().next() {
            Some(m) => Ok(m),
            None => bail!("nenhuma execução registrada para `{task}`"),
        },
    }
}
