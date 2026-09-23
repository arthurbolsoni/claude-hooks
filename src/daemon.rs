use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Local};
use croner::Cron;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::config::{self, Settings, Task, Trigger};
use crate::runner::{self, Event};
use crate::state;
use crate::trigger::{self, CheckSpec};
use crate::util::clip;

const RELOAD_EVERY: Duration = Duration::from_secs(5);
const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(60);

pub struct App {
    settings: Settings,
    sem: Semaphore,
    running: Mutex<HashSet<String>>,
    tasks: RwLock<BTreeMap<String, Task>>,
}

pub enum Fire {
    Started,
    AlreadyRunning,
    Cooldown(Duration),
}

impl App {
    /// Dispara a sessão do Claude respeitando cooldown e uma execução por tarefa.
    pub fn try_fire(self: &Arc<Self>, name: &str, task: Task, event: Event) -> Fire {
        let now = Local::now();
        {
            let mut running = self.running.lock().unwrap();
            if running.contains(name) {
                return Fire::AlreadyRunning;
            }
            let cd = task.effective_cooldown();
            if !cd.is_zero() {
                if let Some(last) = state::load(name).last_fired {
                    let elapsed = (now - last).to_std().unwrap_or_default();
                    if elapsed < cd {
                        return Fire::Cooldown(cd - elapsed);
                    }
                }
            }
            running.insert(name.to_string());
        }
        if let Err(e) = state::update(name, |s| s.last_fired = Some(now)) {
            warn!(task = name, "falha ao gravar estado: {e:#}");
        }

        let app = self.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let _permit = app.sem.acquire().await;
            info!(task = %name, source = %event.source, "iniciando sessão do Claude: {}", event.summary);
            match runner::execute(&app.settings, &name, &task, event).await {
                Ok(m) => info!(
                    task = %name,
                    run = %m.run_id,
                    status = %m.status,
                    cost_usd = m.cost_usd.unwrap_or_default(),
                    session = m.session_id.as_deref().unwrap_or("-"),
                    "sessão finalizada"
                ),
                Err(e) => error!(task = %name, "falha na execução: {e:#}"),
            }
            app.running.lock().unwrap().remove(&name);
        });
        Fire::Started
    }

    fn fire_logged(self: &Arc<Self>, name: &str, task: Task, event: Event) {
        match self.try_fire(name, task, event) {
            Fire::Started => {}
            Fire::AlreadyRunning => info!(task = name, "ignorado: sessão anterior ainda em execução"),
            Fire::Cooldown(left) => info!(
                task = name,
                "ignorado: cooldown, faltam {}",
                humantime::format_duration(Duration::from_secs(left.as_secs()))
            ),
        }
    }
}

#[derive(Default)]
struct Sched {
    fingerprint: String,
    cron: Option<Cron>,
    next: Option<DateTime<Local>>,
    checking: Arc<AtomicBool>,
}

pub async fn run(settings: Settings) -> Result<()> {
    config::ensure_dirs()?;
    let listen = settings.listen.clone();
    let app = Arc::new(App {
        sem: Semaphore::new(settings.max_concurrent.max(1)),
        settings,
        running: Mutex::new(HashSet::new()),
        tasks: RwLock::new(BTreeMap::new()),
    });

    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/tasks", get(list_tasks))
        .route("/hooks/{name}", post(webhook))
        .with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("não foi possível escutar em {listen}"))?;
    info!("claude-hooks daemon | home {} | webhooks em http://{listen}/hooks/<tarefa>", config::home().display());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            error!("servidor HTTP parou: {e}");
        }
    });

    let scheduler = tokio::spawn(scheduler(app));
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("encerrando"),
        r = scheduler => { r?; }
    }
    Ok(())
}

async fn scheduler(app: Arc<App>) {
    let mut sched: HashMap<String, Sched> = HashMap::new();
    let mut last_reload: Option<Instant> = None;
    let mut last_errors: Vec<String> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tick.tick().await;

        if last_reload.is_none_or(|t| t.elapsed() >= RELOAD_EVERY) {
            let (loaded, errors) = config::load_all();
            if errors != last_errors {
                for e in &errors {
                    warn!("{e}");
                }
                last_errors = errors;
            }
            {
                let mut cur = app.tasks.write().unwrap();
                for k in loaded.keys().filter(|k| !cur.contains_key(*k)) {
                    info!(task = %k, trigger = %loaded[k].trigger_label(), enabled = loaded[k].enabled, "tarefa carregada");
                }
                for k in cur.keys().filter(|k| !loaded.contains_key(*k)) {
                    info!(task = %k, "tarefa removida");
                }
                *cur = loaded;
            }
            last_reload = Some(Instant::now());
        }

        let tasks = app.tasks.read().unwrap().clone();
        sched.retain(|k, _| tasks.contains_key(k));
        let now = Local::now();

        for (name, task) in tasks {
            if !task.enabled {
                sched.remove(&name);
                continue;
            }
            let fp = serde_json::to_string(&task.trigger).unwrap_or_default();
            let s = sched.entry(name.clone()).or_default();
            if s.fingerprint != fp {
                *s = Sched { fingerprint: fp, ..Default::default() };
            }

            match &task.trigger {
                Trigger::Cron { schedule } => {
                    if s.cron.is_none() {
                        s.cron = trigger::parse_cron(schedule).ok();
                    }
                    let Some(cron) = &s.cron else { continue };
                    let next = *s.next.get_or_insert_with(|| trigger::next_cron(cron, &now).unwrap_or(now + chrono::Duration::days(3650)));
                    if now >= next {
                        s.next = trigger::next_cron(cron, &now);
                        let event = Event {
                            source: "cron".into(),
                            summary: format!("agendamento `{schedule}` ({})", next.format("%Y-%m-%d %H:%M")),
                            details: None,
                        };
                        app.fire_logged(&name, task.clone(), event);
                    }
                }
                Trigger::Command { interval, .. } => {
                    if s.checking.load(Ordering::SeqCst) || s.next.is_some_and(|n| now < n) {
                        continue;
                    }
                    s.next = Some(now + chrono::Duration::from_std(*interval).unwrap_or(chrono::Duration::minutes(5)));
                    s.checking.store(true, Ordering::SeqCst);
                    let flag = s.checking.clone();
                    let app = app.clone();
                    tokio::spawn(async move {
                        check_and_fire(&app, &name, task).await;
                        flag.store(false, Ordering::SeqCst);
                    });
                }
                Trigger::Webhook { .. } => {}
            }
        }
    }
}

async fn check_and_fire(app: &Arc<App>, name: &str, task: Task) {
    let Trigger::Command { command, when, pattern, shell, timeout, .. } = &task.trigger else { return };
    let spec = CheckSpec {
        command,
        shell: shell.as_deref(),
        timeout: timeout.unwrap_or(DEFAULT_CHECK_TIMEOUT),
        when: *when,
        pattern: pattern.as_deref(),
    };
    let previous = state::load(name).last_output;
    let outcome = match trigger::run_check(&spec, previous.as_deref()).await {
        Ok(o) => o,
        Err(e) => {
            warn!(task = name, "checagem falhou: {e:#}");
            return;
        }
    };
    let output = outcome.output.clone();
    if let Err(e) = state::update(name, |s| {
        s.last_check = Some(Local::now());
        s.last_output = Some(output);
    }) {
        warn!(task = name, "falha ao gravar estado: {e:#}");
    }
    if !outcome.fire {
        tracing::debug!(task = name, "checagem sem disparo: {}", outcome.reason);
        return;
    }
    let event = Event {
        source: "command".into(),
        summary: outcome.reason.clone(),
        details: Some(outcome.details(command)),
    };
    app.fire_logged(name, task.clone(), event);
}

async fn list_tasks(State(app): State<Arc<App>>) -> Json<Value> {
    let tasks = app.tasks.read().unwrap();
    let running = app.running.lock().unwrap();
    let list: Vec<Value> = tasks
        .iter()
        .map(|(n, t)| {
            json!({
                "name": n,
                "enabled": t.enabled,
                "trigger": t.trigger_label(),
                "running": running.contains(n),
                "last_fired": state::load(n).last_fired,
            })
        })
        .collect();
    Json(json!(list))
}

async fn webhook(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let task = app.tasks.read().unwrap().get(&name).cloned();
    let Some(task) = task.filter(|t| t.enabled) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "tarefa não encontrada ou desabilitada"})));
    };
    let Trigger::Webhook { token } = &task.trigger else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "tarefa não é do tipo webhook"})));
    };
    if let Some(expected) = token {
        let given = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .or_else(|| headers.get("x-hook-token").and_then(|v| v.to_str().ok()))
            .or_else(|| q.get("token").map(String::as_str));
        if given != Some(expected.as_str()) {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "token inválido"})));
        }
    }

    let text = String::from_utf8_lossy(&body);
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("-");
    let query: Vec<String> = q.iter().filter(|(k, _)| k.as_str() != "token").map(|(k, v)| format!("{k}={v}")).collect();
    let mut details = format!("content-type: {content_type}\n");
    if !query.is_empty() {
        details.push_str(&format!("query: {}\n", query.join("&")));
    }
    details.push('\n');
    details.push_str(&clip(&text, 50_000));
    let event = Event { source: "webhook".into(), summary: format!("POST /hooks/{name}"), details: Some(details) };

    match app.try_fire(&name, task, event) {
        Fire::Started => (StatusCode::ACCEPTED, Json(json!({"status": "started"}))),
        Fire::AlreadyRunning => (StatusCode::CONFLICT, Json(json!({"status": "already_running"}))),
        Fire::Cooldown(left) => (StatusCode::TOO_MANY_REQUESTS, Json(json!({"status": "cooldown", "retry_after_secs": left.as_secs()}))),
    }
}
