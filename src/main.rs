mod config;
mod daemon;
mod runner;
mod runs;
mod state;
mod trigger;
mod util;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

use config::{ClaudeOpts, Notify, Task, Trigger, When};
use runner::Event;

#[derive(Parser)]
#[command(name = "claude-hooks", version, about = "Tarefas de monitoramento que disparam sessões do Claude Code")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Cria a estrutura em ~/.claude-hooks e um config.toml padrão
    Init,
    /// Roda o agendador e o servidor de webhooks
    Daemon,
    /// Cadastra uma tarefa a partir de flags
    Add(AddArgs),
    /// Cadastra uma tarefa a partir de um arquivo TOML
    Import {
        file: PathBuf,
        /// Nome da tarefa (padrão: nome do arquivo)
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Lista as tarefas
    List,
    /// Mostra o TOML de uma tarefa
    Show { name: String },
    /// Remove uma tarefa (o histórico de execuções é mantido)
    Remove { name: String },
    Enable { name: String },
    Disable { name: String },
    /// Executa o comando de checagem de uma tarefa `command` sem disparar o Claude
    Check { name: String },
    /// Dispara a sessão do Claude agora, ignorando gatilho e cooldown
    Run {
        name: String,
        /// Texto extra passado como dados do evento
        #[arg(long)]
        context: Option<String>,
    },
    /// Histórico de execuções
    Runs {
        name: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Mostra o relatório de uma execução (padrão: a mais recente)
    Result { name: String, run_id: Option<String> },
    /// Mostra o diretório base
    Home,
}

#[derive(Args)]
struct AddArgs {
    name: String,
    /// O que o Claude deve analisar quando disparar
    #[arg(long, conflicts_with = "prompt_file")]
    prompt: Option<String>,
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    #[arg(long)]
    description: Option<String>,

    /// Gatilho cron, ex.: "0 8 * * MON-FRI"
    #[arg(long, group = "trigger")]
    cron: Option<String>,
    /// Gatilho por comando de checagem
    #[arg(long, group = "trigger")]
    command: Option<String>,
    /// Gatilho por webhook (POST /hooks/<nome>)
    #[arg(long, group = "trigger")]
    webhook: bool,

    /// Intervalo da checagem (gatilho command)
    #[arg(long, default_value = "5m")]
    every: humantime::Duration,
    /// Condição de disparo (gatilho command)
    #[arg(long, value_enum, default_value_t = When::ExitNonzero)]
    when: When,
    /// Regex para output_matches / output_not_matches
    #[arg(long)]
    pattern: Option<String>,
    /// Token exigido no webhook
    #[arg(long)]
    token: Option<String>,

    #[arg(long)]
    cooldown: Option<humantime::Duration>,
    #[arg(long)]
    timeout: Option<humantime::Duration>,

    /// Diretório onde a sessão do Claude roda
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    permission_mode: Option<String>,
    /// Ferramentas liberadas, ex.: --allow "Bash(ssh *)" --allow Read
    #[arg(long = "allow")]
    allowed_tools: Vec<String>,
    #[arg(long)]
    max_budget_usd: Option<f64>,

    /// URL que recebe o resultado em JSON
    #[arg(long)]
    notify_webhook: Option<String>,
    /// Comando executado ao fim da sessão
    #[arg(long)]
    notify_command: Option<String>,

    #[arg(long)]
    force: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CLAUDE_HOOKS_LOG").unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => init(),
        Cmd::Daemon => daemon::run(config::load_settings()?).await,
        Cmd::Add(a) => add(a),
        Cmd::Import { file, name, force } => import(file, name, force),
        Cmd::List => list(),
        Cmd::Show { name } => {
            let p = config::task_path(&name);
            print!("{}", std::fs::read_to_string(&p).with_context(|| format!("tarefa `{name}` não encontrada"))?);
            Ok(())
        }
        Cmd::Remove { name } => {
            let p = config::task_path(&name);
            std::fs::remove_file(&p).with_context(|| format!("tarefa `{name}` não encontrada"))?;
            state::remove(&name);
            println!("removida: {name}");
            Ok(())
        }
        Cmd::Enable { name } => set_enabled(&name, true),
        Cmd::Disable { name } => set_enabled(&name, false),
        Cmd::Check { name } => check(&name).await,
        Cmd::Run { name, context } => run_now(&name, context).await,
        Cmd::Runs { name, limit } => {
            print_runs(name.as_deref(), limit);
            Ok(())
        }
        Cmd::Result { name, run_id } => show_result(&name, run_id.as_deref()),
        Cmd::Home => {
            println!("{}", config::home().display());
            Ok(())
        }
    }
}

fn init() -> Result<()> {
    config::ensure_dirs()?;
    let p = config::settings_path();
    if !p.exists() {
        std::fs::write(&p, toml::to_string_pretty(&config::Settings::default())?)?;
        println!("criado {}", p.display());
    }
    println!("tarefas em {}", config::tasks_dir().display());
    Ok(())
}

fn add(a: AddArgs) -> Result<()> {
    config::valid_name(&a.name)?;
    if config::task_path(&a.name).exists() && !a.force {
        bail!("tarefa `{}` já existe (use --force para sobrescrever)", a.name);
    }
    let prompt = match (a.prompt, a.prompt_file) {
        (Some(p), _) => p,
        (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("lendo {}", f.display()))?,
        (None, None) => bail!("informe --prompt ou --prompt-file"),
    };
    let trigger = if let Some(schedule) = a.cron {
        Trigger::Cron { schedule }
    } else if let Some(command) = a.command {
        Trigger::Command {
            command,
            interval: a.every.into(),
            when: a.when,
            pattern: a.pattern,
            shell: None,
            timeout: None,
        }
    } else if a.webhook {
        Trigger::Webhook { token: a.token }
    } else {
        bail!("informe um gatilho: --cron, --command ou --webhook");
    };
    let notify = (a.notify_webhook.is_some() || a.notify_command.is_some()).then(|| Notify {
        webhook: a.notify_webhook,
        command: a.notify_command,
        ..Default::default()
    });
    let task = Task {
        description: a.description,
        enabled: true,
        prompt,
        cooldown: a.cooldown.map(Duration::from),
        timeout: a.timeout.map(Duration::from),
        trigger,
        claude: ClaudeOpts {
            cwd: a.cwd,
            model: a.model,
            permission_mode: a.permission_mode,
            allowed_tools: a.allowed_tools,
            max_budget_usd: a.max_budget_usd,
            ..Default::default()
        },
        notify,
    };
    let p = config::save_task(&a.name, &task)?;
    println!("tarefa `{}` gravada em {}", a.name, p.display());
    Ok(())
}

fn import(file: PathBuf, name: Option<String>, force: bool) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => file
            .file_stem()
            .and_then(|s| s.to_str())
            .map(String::from)
            .context("não foi possível derivar o nome do arquivo")?,
    };
    config::valid_name(&name)?;
    let text = std::fs::read_to_string(&file).with_context(|| format!("lendo {}", file.display()))?;
    config::parse_task(&text, &file)?;
    let dest = config::task_path(&name);
    if dest.exists() && !force {
        bail!("tarefa `{name}` já existe (use --force para sobrescrever)");
    }
    config::ensure_dirs()?;
    std::fs::write(&dest, text)?;
    println!("tarefa `{name}` gravada em {}", dest.display());
    Ok(())
}

fn list() -> Result<()> {
    let (tasks, errors) = config::load_all();
    if tasks.is_empty() && errors.is_empty() {
        println!("nenhuma tarefa em {}", config::tasks_dir().display());
    }
    for (name, t) in &tasks {
        let st = state::load(name);
        let last = st.last_fired.map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "-".into());
        println!(
            "{:<24} {:<4} {:<40} último disparo: {}",
            name,
            if t.enabled { "on" } else { "off" },
            t.trigger_label(),
            last
        );
        if let Some(d) = &t.description {
            println!("    {d}");
        }
    }
    for e in errors {
        eprintln!("erro: {e}");
    }
    Ok(())
}

fn set_enabled(name: &str, enabled: bool) -> Result<()> {
    // Edita só a chave `enabled` para preservar comentários e formatação do arquivo.
    let p = config::task_path(name);
    let text = std::fs::read_to_string(&p).with_context(|| format!("tarefa `{name}` não encontrada"))?;
    let mut out = String::new();
    let mut replaced = false;
    for line in text.lines() {
        if !replaced && line.trim_start().starts_with("enabled") && line.contains('=') {
            out.push_str(&format!("enabled = {enabled}\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out = format!("enabled = {enabled}\n{text}");
    }
    config::parse_task(&out, &p)?;
    std::fs::write(&p, out)?;
    println!("{name}: {}", if enabled { "habilitada" } else { "desabilitada" });
    Ok(())
}

async fn check(name: &str) -> Result<()> {
    let task = config::load_task(name)?;
    let Trigger::Command { command, when, pattern, shell, timeout, .. } = &task.trigger else {
        bail!("`{name}` não é uma tarefa do tipo command");
    };
    let spec = trigger::CheckSpec {
        command,
        shell: shell.as_deref(),
        timeout: timeout.unwrap_or(Duration::from_secs(60)),
        when: *when,
        pattern: pattern.as_deref(),
    };
    let prev = state::load(name).last_output;
    let o = trigger::run_check(&spec, prev.as_deref()).await?;
    println!("{}", o.details(command));
    println!("---\ndispararia: {} ({})", if o.fire { "sim" } else { "não" }, o.reason);
    Ok(())
}

async fn run_now(name: &str, context: Option<String>) -> Result<()> {
    let task = config::load_task(name)?;
    let settings = config::load_settings()?;
    let event = Event { source: "manual".into(), summary: "execução manual via CLI".into(), details: context };
    eprintln!("rodando sessão do Claude para `{name}`...");
    let meta = runner::execute(&settings, name, &task, event).await?;
    let dir = runs::run_dir(name, &meta.run_id);
    if let Ok(r) = std::fs::read_to_string(dir.join("result.md")) {
        println!("{r}");
    }
    eprintln!("\nstatus: {} | run: {} | dir: {}", meta.status, meta.run_id, dir.display());
    if let Some(e) = &meta.error {
        eprintln!("erro: {e}");
    }
    print_resume_hint(&meta);
    Ok(())
}

fn print_runs(name: Option<&str>, limit: usize) {
    let list = runs::list(name);
    if list.is_empty() {
        println!("nenhuma execução registrada");
    }
    for m in list.into_iter().take(limit) {
        let dur = m
            .finished_at
            .map(|f| humantime::format_duration(Duration::from_secs((f - m.started_at).num_seconds().max(0) as u64)).to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<24} {:<18} {:<8} {:>8} ${:<7.4} {} — {}",
            m.task,
            m.run_id,
            m.status.to_string(),
            dur,
            m.cost_usd.unwrap_or_default(),
            m.event.source,
            m.event.summary
        );
    }
}

fn show_result(name: &str, run_id: Option<&str>) -> Result<()> {
    let meta = runs::find(name, run_id)?;
    let dir = runs::run_dir(name, &meta.run_id);
    println!("# {} / {} ({})\n", meta.task, meta.run_id, meta.status);
    match std::fs::read_to_string(dir.join("result.md")) {
        Ok(r) => println!("{r}"),
        Err(_) => println!("(sem result.md)"),
    }
    if let Some(e) = &meta.error {
        println!("\nerro: {e}");
    }
    print_resume_hint(&meta);
    Ok(())
}

fn print_resume_hint(meta: &runner::RunMeta) {
    if let Some(sid) = &meta.session_id {
        let cd = meta.cwd.as_ref().map(|c| format!("cd \"{}\" && ", c.display())).unwrap_or_default();
        eprintln!("continuar a análise: {cd}claude --resume {sid}");
    }
}
