use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use croner::Cron;
use regex::Regex;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;
use tokio::process::Command;

use crate::config::When;
use crate::util::clip;

pub fn parse_cron(expr: &str) -> Result<Cron> {
    Cron::from_str(expr).with_context(|| format!("expressão cron inválida: `{expr}`"))
}

pub fn next_cron(cron: &Cron, after: &DateTime<Local>) -> Option<DateTime<Local>> {
    cron.find_next_occurrence(after, false).ok()
}

/// Monta o processo que executa `cmd` no shell configurado.
pub fn shell_command(shell: Option<&[String]>, cmd: &str) -> Command {
    match shell {
        Some([prog, args @ ..]) => {
            let mut c = Command::new(prog);
            c.args(args).arg(cmd);
            c
        }
        _ => default_shell(cmd),
    }
}

#[cfg(windows)]
fn default_shell(cmd: &str) -> Command {
    // cmd.exe não segue as regras de aspas do CRT; a linha vai crua.
    let mut c = Command::new("cmd");
    c.arg("/C").raw_arg(cmd);
    c
}

#[cfg(not(windows))]
fn default_shell(cmd: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    c
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output: String,
    pub fire: bool,
    pub reason: String,
}

impl CheckOutcome {
    pub fn details(&self, command: &str) -> String {
        let code = match (self.timed_out, self.exit_code) {
            (true, _) => "timeout".to_string(),
            (_, Some(c)) => c.to_string(),
            (_, None) => "encerrado por sinal".to_string(),
        };
        format!("$ {command}\nexit code: {code}\n\n{}", clip(&self.output, 50_000))
    }
}

pub struct CheckSpec<'a> {
    pub command: &'a str,
    pub shell: Option<&'a [String]>,
    pub timeout: Duration,
    pub when: When,
    pub pattern: Option<&'a str>,
}

/// Executa o comando de checagem e avalia a condição de disparo.
pub async fn run_check(spec: &CheckSpec<'_>, previous_output: Option<&str>) -> Result<CheckOutcome> {
    let mut cmd = shell_command(spec.shell, spec.command);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().with_context(|| format!("falha ao executar `{}`", spec.command))?;

    let (exit_code, timed_out, output) = match tokio::time::timeout(spec.timeout, child.wait_with_output()).await {
        Err(_) => (None, true, String::new()),
        Ok(res) => {
            let out = res?;
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            (out.status.code(), false, text)
        }
    };

    let failed = timed_out || exit_code != Some(0);
    let matches = || -> Result<bool> {
        let re = Regex::new(spec.pattern.unwrap_or_default())?;
        Ok(re.is_match(&output))
    };
    let (fire, reason) = match spec.when {
        When::ExitNonzero => (failed, if failed { "comando falhou".into() } else { "comando ok".into() }),
        When::ExitZero => (!failed, if failed { "comando falhou".into() } else { "comando ok".into() }),
        When::OutputMatches => {
            let m = matches()?;
            (m, format!("pattern {}", if m { "encontrado" } else { "ausente" }))
        }
        When::OutputNotMatches => {
            let m = matches()?;
            (!m, format!("pattern {}", if m { "encontrado" } else { "ausente" }))
        }
        When::OutputChanged => match previous_output {
            None => (false, "primeira checagem, saída registrada como referência".into()),
            Some(prev) if prev != output => (true, "saída mudou".into()),
            Some(_) => (false, "saída igual à anterior".into()),
        },
        When::Always => (true, "always".into()),
    };

    Ok(CheckOutcome { exit_code, timed_out, output, fire, reason })
}
