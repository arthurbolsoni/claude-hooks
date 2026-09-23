use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{self, ClaudeOpts, Notify, NotifyOn, Settings, Task};
use crate::trigger::shell_command;
use crate::util::clip;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const MAX_DETAILS: usize = 60_000;

/// O que causou o disparo, repassado ao Claude como contexto.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// cron | command | webhook | manual
    pub source: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Ok,
    Error,
    Timeout,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            RunStatus::Running => "running",
            RunStatus::Ok => "ok",
            RunStatus::Error => "error",
            RunStatus::Timeout => "timeout",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub task: String,
    pub run_id: String,
    pub status: RunStatus,
    pub started_at: DateTime<Local>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Local>>,
    pub event: Event,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub num_turns: Option<u64>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeJson {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    num_turns: Option<u64>,
    #[serde(default)]
    subtype: Option<String>,
}

pub fn build_prompt(name: &str, task: &Task, event: &Event, at: &DateTime<Local>) -> String {
    let mut p = String::new();
    p.push_str(task.prompt.trim());
    p.push_str("\n\n---\n## Contexto do disparo (claude-hooks)\n\n");
    p.push_str(&format!("- Tarefa: `{name}`\n"));
    if let Some(d) = &task.description {
        p.push_str(&format!("- Descrição: {d}\n"));
    }
    p.push_str(&format!("- Disparada em: {}\n", at.format("%Y-%m-%d %H:%M:%S %:z")));
    p.push_str(&format!("- Origem: {} — {}\n", event.source, event.summary));
    if let Some(d) = event.details.as_deref().filter(|d| !d.trim().is_empty()) {
        p.push_str("\n### Dados do evento\n\n```text\n");
        p.push_str(&clip(d, MAX_DETAILS));
        if !d.ends_with('\n') {
            p.push('\n');
        }
        p.push_str("```\n");
    }
    p.push_str(
        "\nEsta sessão é não interativa: não há ninguém para responder perguntas. \
         Siga o formato de resposta pedido acima; se nenhum foi pedido, termine com um relatório \
         objetivo em markdown (o que foi encontrado, causa provável e ação recomendada).\n",
    );
    p
}

fn create_run_dir(name: &str, at: &DateTime<Local>) -> Result<(String, PathBuf)> {
    let base = config::runs_dir().join(name);
    std::fs::create_dir_all(&base)?;
    let stamp = at.format("%Y%m%d-%H%M%S").to_string();
    for i in 0..100 {
        let id = if i == 0 { stamp.clone() } else { format!("{stamp}-{i}") };
        let dir = base.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("não foi possível criar diretório de execução em {}", base.display())
}

fn write_meta(dir: &Path, meta: &RunMeta) -> Result<()> {
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(meta)?)?;
    Ok(())
}

/// Executa uma sessão do Claude Code para a tarefa e registra tudo em `runs/<tarefa>/<id>/`.
pub async fn execute(settings: &Settings, name: &str, task: &Task, event: Event) -> Result<RunMeta> {
    let started = Local::now();
    let (run_id, dir) = create_run_dir(name, &started)?;
    let prompt = build_prompt(name, task, &event, &started);
    std::fs::write(dir.join("prompt.md"), &prompt)?;

    let mut meta = RunMeta {
        task: name.to_string(),
        run_id,
        status: RunStatus::Running,
        started_at: started,
        finished_at: None,
        event,
        session_id: None,
        cost_usd: None,
        num_turns: None,
        cwd: task.claude.cwd.clone(),
        error: None,
    };
    write_meta(&dir, &meta)?;

    let timeout = task.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let mut result_text: Option<String> = None;
    match tokio::time::timeout(timeout, invoke_claude(settings, name, &task.claude, &prompt, &dir)).await {
        Err(_) => {
            meta.status = RunStatus::Timeout;
            meta.error = Some(format!("sessão excedeu {}", humantime::format_duration(timeout)));
        }
        Ok(Err(e)) => {
            meta.status = RunStatus::Error;
            meta.error = Some(format!("{e:#}"));
        }
        Ok(Ok(out)) => {
            meta.session_id = out.session_id;
            meta.cost_usd = out.total_cost_usd;
            meta.num_turns = out.num_turns;
            let failed = out.is_error.unwrap_or(false) || out.subtype.as_deref().is_some_and(|s| s != "success");
            meta.status = if failed { RunStatus::Error } else { RunStatus::Ok };
            if failed {
                meta.error = out.subtype.clone().or(Some("claude retornou is_error".into()));
            }
            result_text = out.result;
        }
    }

    if let Some(r) = &result_text {
        std::fs::write(dir.join("result.md"), r)?;
    }
    meta.finished_at = Some(Local::now());
    write_meta(&dir, &meta)?;

    if let Some(n) = &task.notify {
        notify(n, &meta, &dir, result_text.as_deref()).await;
    }
    Ok(meta)
}

async fn invoke_claude(settings: &Settings, name: &str, opts: &ClaudeOpts, prompt: &str, run_dir: &Path) -> Result<ClaudeJson> {
    let mut cmd = Command::new(&settings.claude_bin);
    cmd.arg("-p")
        .args(["--output-format", "json"])
        .args(["--name", &format!("claude-hooks: {name}")]);
    if let Some(m) = &opts.model {
        cmd.args(["--model", m]);
    }
    if let Some(m) = &opts.permission_mode {
        cmd.args(["--permission-mode", m]);
    }
    if !opts.allowed_tools.is_empty() {
        cmd.arg("--allowed-tools").arg(opts.allowed_tools.join(","));
    }
    if !opts.disallowed_tools.is_empty() {
        cmd.arg("--disallowed-tools").arg(opts.disallowed_tools.join(","));
    }
    for d in &opts.add_dirs {
        cmd.arg("--add-dir").arg(d);
    }
    if let Some(s) = &opts.append_system_prompt {
        cmd.args(["--append-system-prompt", s]);
    }
    if let Some(b) = opts.max_budget_usd {
        cmd.args(["--max-budget-usd", &b.to_string()]);
    }
    if let Some(e) = &opts.effort {
        cmd.args(["--effort", e]);
    }
    cmd.args(&opts.extra_args);
    if let Some(cwd) = &opts.cwd {
        cmd.current_dir(cwd);
    }
    cmd.env("CLAUDE_HOOKS_TASK", name)
        .env("CLAUDE_HOOKS_RUN_DIR", run_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("falha ao iniciar `{}` (claude_bin em config.toml)", settings.claude_bin))?;
    // O prompt vai por stdin: evita limite de tamanho e problemas de aspas na linha de comando.
    let mut stdin = child.stdin.take().expect("stdin piped");
    let body = prompt.to_string();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(body.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let out = child.wait_with_output().await?;
    let _ = writer.await;

    std::fs::write(run_dir.join("claude-stdout.json"), &out.stdout)?;
    if !out.stderr.is_empty() {
        std::fs::write(run_dir.join("claude-stderr.log"), &out.stderr)?;
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    match serde_json::from_str::<ClaudeJson>(stdout.trim()) {
        Ok(j) => Ok(j),
        Err(_) => bail!(
            "claude saiu com {} sem JSON válido. stderr: {}",
            out.status,
            clip(String::from_utf8_lossy(&out.stderr).trim(), 2000)
        ),
    }
}

async fn notify(n: &Notify, meta: &RunMeta, dir: &Path, result: Option<&str>) {
    let ok = meta.status == RunStatus::Ok;
    let wanted = match n.on {
        NotifyOn::Always => true,
        NotifyOn::Success => ok,
        NotifyOn::Failure => !ok,
    };
    if !wanted {
        return;
    }

    if let Some(url) = &n.webhook {
        let payload = serde_json::json!({
            "task": meta.task,
            "run_id": meta.run_id,
            "status": meta.status,
            "event": meta.event,
            "result": result,
            "error": meta.error,
            "session_id": meta.session_id,
            "cost_usd": meta.cost_usd,
            "started_at": meta.started_at,
            "finished_at": meta.finished_at,
            "run_dir": dir,
        });
        let res = reqwest::Client::new()
            .post(url)
            .timeout(Duration::from_secs(30))
            .json(&payload)
            .send()
            .await
            .and_then(|r| r.error_for_status());
        if let Err(e) = res {
            tracing::warn!(task = %meta.task, "notify webhook falhou: {e}");
        }
    }

    if let Some(c) = &n.command {
        let mut cmd = shell_command(None, c);
        cmd.env("CLAUDE_HOOKS_TASK", &meta.task)
            .env("CLAUDE_HOOKS_RUN_ID", &meta.run_id)
            .env("CLAUDE_HOOKS_STATUS", meta.status.to_string())
            .env("CLAUDE_HOOKS_RUN_DIR", dir)
            .env("CLAUDE_HOOKS_RESULT_FILE", dir.join("result.md"))
            .env("CLAUDE_HOOKS_SESSION_ID", meta.session_id.clone().unwrap_or_default())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(120), cmd.status()).await {
            Ok(Ok(s)) if s.success() => {}
            Ok(Ok(s)) => tracing::warn!(task = %meta.task, "notify command saiu com {s}"),
            Ok(Err(e)) => tracing::warn!(task = %meta.task, "notify command falhou: {e}"),
            Err(_) => tracing::warn!(task = %meta.task, "notify command excedeu 120s"),
        }
    }
}
