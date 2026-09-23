use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::trigger;

/// Diretório raiz do claude-hooks (`CLAUDE_HOOKS_HOME` ou `~/.claude-hooks`).
pub fn home() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_HOOKS_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .expect("não foi possível determinar o diretório home")
        .join(".claude-hooks")
}

pub fn tasks_dir() -> PathBuf {
    home().join("tasks")
}

pub fn runs_dir() -> PathBuf {
    home().join("runs")
}

pub fn state_dir() -> PathBuf {
    home().join("state")
}

pub fn ensure_dirs() -> Result<()> {
    for d in [tasks_dir(), runs_dir(), state_dir()] {
        std::fs::create_dir_all(&d).with_context(|| format!("criando {}", d.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Configuração global (config.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Endereço do servidor HTTP de webhooks.
    pub listen: String,
    /// Binário do Claude Code.
    pub claude_bin: String,
    /// Quantas sessões do Claude podem rodar ao mesmo tempo.
    pub max_concurrent: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7878".into(),
            claude_bin: "claude".into(),
            max_concurrent: 2,
        }
    }
}

pub fn settings_path() -> PathBuf {
    home().join("config.toml")
}

pub fn load_settings() -> Result<Settings> {
    let p = settings_path();
    if !p.exists() {
        return Ok(Settings::default());
    }
    let s = std::fs::read_to_string(&p).with_context(|| format!("lendo {}", p.display()))?;
    toml::from_str(&s).with_context(|| format!("config inválida em {}", p.display()))
}

// ---------------------------------------------------------------------------
// Tarefas
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// O que o Claude deve analisar quando a tarefa disparar.
    pub prompt: String,
    /// Intervalo mínimo entre dois disparos. Padrão: 30m para `command`, 0 para os demais.
    #[serde(default, with = "humantime_serde", skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<Duration>,
    /// Tempo máximo da sessão do Claude. Padrão: 20m.
    #[serde(default, with = "humantime_serde", skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    pub trigger: Trigger,
    #[serde(default)]
    pub claude: ClaudeOpts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<Notify>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Dispara em horários fixos (cron de 5 campos, ou 6 com segundos).
    Cron { schedule: String },
    /// Roda um comando a cada `interval` e dispara quando `when` for verdadeiro.
    Command {
        command: String,
        #[serde(with = "humantime_serde")]
        interval: Duration,
        #[serde(default)]
        when: When,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        /// Ex.: ["bash", "-c"]. Padrão: `cmd /C` no Windows, `sh -c` nos demais.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<Vec<String>>,
        /// Tempo máximo do comando de checagem. Padrão: 60s.
        #[serde(default, with = "humantime_serde", skip_serializing_if = "Option::is_none")]
        timeout: Option<Duration>,
    },
    /// Dispara em `POST /hooks/<nome>`.
    Webhook {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum When {
    #[default]
    ExitNonzero,
    ExitZero,
    OutputMatches,
    OutputNotMatches,
    OutputChanged,
    Always,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeOpts {
    /// Diretório onde a sessão roda (CLAUDE.md, skills e settings do projeto valem).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// default | acceptEdits | plan | bypassPermissions ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_dirs: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Argumentos extras repassados ao `claude`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    /// URL que recebe um POST JSON com o resultado.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    /// Comando executado ao fim (variáveis CLAUDE_HOOKS_* no ambiente).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub on: NotifyOn,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotifyOn {
    #[default]
    Always,
    Success,
    Failure,
}

impl Task {
    pub fn validate(&self) -> Result<()> {
        if self.prompt.trim().is_empty() {
            bail!("`prompt` vazio");
        }
        match &self.trigger {
            Trigger::Cron { schedule } => {
                trigger::parse_cron(schedule)?;
            }
            Trigger::Command { when, pattern, interval, .. } => {
                if interval.is_zero() {
                    bail!("`interval` precisa ser maior que zero");
                }
                let needs = matches!(when, When::OutputMatches | When::OutputNotMatches);
                match (needs, pattern) {
                    (true, None) => bail!("`when = {:?}` exige `pattern`", when),
                    (_, Some(p)) => {
                        Regex::new(p).with_context(|| format!("regex inválida: {p}"))?;
                    }
                    _ => {}
                }
            }
            Trigger::Webhook { .. } => {}
        }
        Ok(())
    }

    pub fn effective_cooldown(&self) -> Duration {
        match (self.cooldown, &self.trigger) {
            (Some(c), _) => c,
            (None, Trigger::Command { .. }) => Duration::from_secs(30 * 60),
            (None, _) => Duration::ZERO,
        }
    }

    pub fn trigger_label(&self) -> String {
        match &self.trigger {
            Trigger::Cron { schedule } => format!("cron `{schedule}`"),
            Trigger::Command { interval, when, .. } => format!(
                "command a cada {} ({})",
                humantime::format_duration(*interval),
                serde_json::to_value(when).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
            ),
            Trigger::Webhook { .. } => "webhook".into(),
        }
    }
}

pub fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("nome inválido `{name}`: use letras, números, `-` e `_` (até 64)");
    }
    Ok(())
}

pub fn task_path(name: &str) -> PathBuf {
    tasks_dir().join(format!("{name}.toml"))
}

pub fn parse_task(text: &str, origin: &Path) -> Result<Task> {
    let task: Task = toml::from_str(text).with_context(|| format!("TOML inválido em {}", origin.display()))?;
    task.validate().with_context(|| format!("tarefa inválida em {}", origin.display()))?;
    Ok(task)
}

pub fn load_task(name: &str) -> Result<Task> {
    let p = task_path(name);
    let text = std::fs::read_to_string(&p).with_context(|| format!("tarefa `{name}` não encontrada ({})", p.display()))?;
    parse_task(&text, &p)
}

pub fn save_task(name: &str, task: &Task) -> Result<PathBuf> {
    valid_name(name)?;
    task.validate()?;
    ensure_dirs()?;
    let p = task_path(name);
    let text = toml::to_string_pretty(task)?;
    std::fs::write(&p, text).with_context(|| format!("gravando {}", p.display()))?;
    Ok(p)
}

/// Carrega todas as tarefas. Arquivos inválidos voltam em `errors` sem abortar o resto.
pub fn load_all() -> (BTreeMap<String, Task>, Vec<String>) {
    let mut tasks = BTreeMap::new();
    let mut errors = Vec::new();
    let Ok(rd) = std::fs::read_dir(tasks_dir()) else {
        return (tasks, errors);
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        if let Err(e) = valid_name(&name) {
            errors.push(format!("{e:#}"));
            continue;
        }
        match std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|t| parse_task(&t, &path)) {
            Ok(t) => {
                tasks.insert(name, t);
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    (tasks, errors)
}
