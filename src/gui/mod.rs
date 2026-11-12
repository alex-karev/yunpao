use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response, sse::{Event, KeepAlive, Sse}},
    routing::{get, post},
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible, fmt::Write as _, net::SocketAddr, path::PathBuf, sync::Arc,
    time::Duration,
};
use tokio::process::Command as Cmd;

use crate::{
    ai::{self, AiSettings, ChatTurn, KeySource},
    config::{
        Global, HostArgs, Project, Server,
        ssh_config::{self, SshHost},
    },
    storage::ProjectState,
};

mod check;
mod gpu_db;
mod jobs;
mod login;
mod queue;
mod terminal;

pub use jobs::record_cli_run;
pub use login::askpass_mode;

const INDEX_HTML: &str = include_str!("index.html");

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Default)]
pub struct GpuProc {
    pub pid: u32,
    pub name: String,
    pub mem_mib: u32,
    /// Owner from `ps`; empty when the pid isn't visible (e.g. it runs in a container).
    pub user: String,
    pub elapsed_secs: Option<u64>,
    /// Full command line, shortened.
    pub cmd: String,
}

#[derive(Serialize, Clone, Default)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub util_gpu: u32,
    pub util_mem: u32,
    pub mem_used: u32,
    pub mem_total: u32,
    pub temp: u32,
    pub power_draw: f32,
    pub power_limit: f32,
    pub procs: Vec<GpuProc>,
    /// Known model: architecture and what it suits (None for models not in the table).
    pub profile: Option<gpu_db::GpuProfile>,
}

#[derive(Serialize)]
struct ServerInfo {
    id: String,
    label: String,
    user: String,
    host: String,
    port: u16,
}

#[derive(Serialize)]
struct TaskInfo {
    name: String,
    pid: String,
}

#[derive(Serialize)]
struct SessionInfo {
    id: String,
    name: String,
    server_id: String,
    server_label: String,
    is_current: bool,
    created_at: String,
    tasks: Vec<TaskInfo>,
}

#[derive(Serialize)]
struct DashboardData {
    project_id: String,
    servers: Vec<ServerInfo>,
    sessions: Vec<SessionInfo>,
}

#[derive(Deserialize)]
struct ChatReq {
    message: String,
    context: Option<String>,
    #[serde(default)]
    history: Vec<ChatTurn>,
}


#[derive(Serialize)]
struct ChatResp {
    reply: String,
    command: Option<String>,
    job: Option<ai::JobSuggestion>,
}

#[derive(Deserialize)]
struct LogsQuery {
    session_id: String,
    action: String,
    /// A specific earlier run (see RemoteContext::run_tracked); latest when absent.
    #[serde(default)]
    run: Option<String>,
}

#[derive(Serialize)]
struct LogsResp {
    lines: Vec<String>,
}

#[derive(Serialize)]
struct SettingsResp {
    base_url: String,
    model: String,
    key_hint: Option<String>,
    key_source: KeySource,
    path: String,
}

#[derive(Deserialize)]
struct SettingsReq {
    /// Empty means "keep the saved key".
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
}

#[derive(Serialize)]
struct TestResp {
    ok: bool,
    message: String,
}

#[derive(Serialize)]
struct SshHostInfo {
    #[serde(flatten)]
    host: SshHost,
    imported: bool,
}

#[derive(Deserialize)]
struct ImportReq {
    aliases: Vec<String>,
}

#[derive(Serialize)]
struct ImportResp {
    ok: bool,
    added: Vec<String>,
    message: String,
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    /// Fallback only; handlers re-read config.toml so CLI/import changes show up live.
    config: Global,
    project_id: String,
    actions: Vec<(String, String)>,
    project_dir: PathBuf,
    port: u16,
}

impl AppState {
    fn global(&self) -> Global {
        Global::load().unwrap_or_else(|_| self.config.clone())
    }
}

/// Prefer a human alias (e.g. "NSEC") over the auto-generated "user@host:port" one.
fn server_label(s: &Server) -> String {
    s.aliases
        .iter()
        .find(|a| !a.contains('@'))
        .or(s.aliases.first())
        .cloned()
        .unwrap_or_else(|| s.id.clone())
}

fn is_safe_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ── Entry ─────────────────────────────────────────────────────────────────────

pub fn serve(config: &Global, project: &Project, port: u16, open_browser: bool) -> Result<()> {
    let project_id = project
        .state
        .get_path()
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut actions: Vec<(String, String)> =
        project.actions.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    actions.sort();

    // Learn each server's home directory in the background (paths depend on it).
    for srv in config.servers.values().cloned() {
        std::thread::spawn(move || crate::ssh::remote_home(&srv));
    }
    let shared = Arc::new(AppState {
        config: config.clone(),
        project_id,
        actions,
        project_dir: std::env::current_dir()?,
        port,
    });

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .with_context(|| {
                format!("Port {port} is already in use. Try another one: yunpao ui --port {}", port + 1)
            })?;

        let app = Router::new()
            .route("/", get(root))
            .route("/api/data", get(api_data))
            .route("/api/gpu2/:server_id", get(api_gpu))
            // Tabs still running an older page keep reconnecting their GPU streams after a
            // restart and hold the browser's 6 connections; refuse them so they let go.
            .route("/api/gpu/:server_id", get(|| async { (StatusCode::GONE, "page outdated, reload") }))
            .route("/api/chat", post(api_chat))
            .route("/api/logs", get(api_logs))
            .route("/api/settings", get(api_settings_get).post(api_settings_save))
            .route("/api/settings/test", post(api_settings_test))
            .route("/api/settings/models", post(api_settings_models))
            .route("/api/ssh-hosts", get(api_ssh_hosts))
            .route("/api/ssh-hosts/import", post(api_ssh_import))
            .route("/api/project", get(jobs::api_project))
            .route("/api/jobs", post(jobs::api_submit))
            .route("/api/jobs/status", get(jobs::api_status))
            .route("/api/jobs/stop", post(jobs::api_stop))
            .route("/api/jobs/diagnose", post(jobs::api_diagnose))
            .route("/api/jobs/preview", post(jobs::api_upload_preview))
            .route("/api/jobs/check", post(check::api_check))
            .route("/api/jobs/install", post(check::api_install))
            .route("/api/jobs/gpu-usage", get(check::api_gpu_usage))
            .route("/api/queue", get(queue::api_queue).post(queue::api_enqueue))
            .route("/api/queue/cancel", post(queue::api_cancel))
            .route("/api/login", post(login::api_login))
            .route("/api/terminal/:server_id", get(terminal::api_terminal))
            .route("/vendor/:file", get(terminal::vendor))
            .route("/api/envs", get(jobs::api_envs))
            .route("/api/history", get(jobs::api_history))
            .route("/api/history/sync", post(jobs::api_history_sync))
            .route("/api/results", get(jobs::api_results))
            .route("/api/results/pull", post(jobs::api_pull))
            .route("/api/preview", get(jobs::api_preview))
            .layer(middleware::from_fn_with_state(shared.clone(), host_guard))
            .with_state(shared);

        let url = format!("http://localhost:{port}");
        println!("  yunpao UI → {url}");
        println!("  Press Ctrl-C to stop\n");

        if open_browser {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = open::that(url);
            });
        }

        axum::serve(listener, app).await.context("Web server stopped unexpectedly")
    })
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn root() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn api_data(State(app): State<Arc<AppState>>) -> Json<DashboardData> {
    let config = app.global();
    let state = ProjectState::load(&app.project_id).unwrap_or_default();
    let current_id = state.current_session().map(|s| s.id.clone());

    let mut servers: Vec<ServerInfo> = config
        .servers
        .values()
        .map(|s| ServerInfo {
            id: s.id.clone(),
            label: server_label(s),
            user: s.user.clone(),
            host: s.host.clone(),
            port: s.port,
        })
        .collect();
    servers.sort_by(|a, b| a.label.cmp(&b.label));

    let mut sessions: Vec<SessionInfo> = state
        .list_sessions()
        .map(|s| {
            let server_label = config
                .servers
                .get(&s.server)
                .map(server_label)
                .unwrap_or_else(|| s.server.clone());
            SessionInfo {
                id: s.id.clone(),
                name: s.name.clone(),
                server_id: s.server.clone(),
                server_label,
                is_current: current_id.as_deref() == Some(s.id.as_str()),
                created_at: s.created_at_str(),
                tasks: s
                    .tasks
                    .iter()
                    .map(|(n, p)| TaskInfo { name: n.clone(), pid: p.clone() })
                    .collect(),
            }
        })
        .collect();
    sessions.sort_by(|a, b| b.is_current.cmp(&a.is_current).then(a.name.cmp(&b.name)));

    Json(DashboardData { project_id: app.project_id.clone(), servers, sessions })
}

async fn api_gpu(
    State(app): State<Arc<AppState>>,
    Path(server_id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let server = app.global().servers.get(&server_id).cloned();

    let stream = async_stream::stream! {
        let mut failures: u32 = 0;
        loop {
            let payload = match &server {
                Some(srv) => match gpu_stats(srv).await {
                    Ok(gpus) => { failures = 0; serde_json::json!({ "gpus": gpus, "error": null }) }
                    Err(e) => { failures += 1; serde_json::json!({ "gpus": [], "error": e }) }
                },
                None => { failures += 1; serde_json::json!({ "gpus": [], "error": "server not found" }) }
            };
            yield Ok::<Event, Infallible>(Event::default().data(payload.to_string()));
            // Back off on an unreachable server instead of hammering it: 3s → 15s → 30s → 60s.
            let wait = match failures { 0 => 3, 1 => 15, 2 => 30, _ => 60 };
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Plain-text picture of the user's real state, given to the AI with every chat message.
fn build_snapshot(app: &AppState, gpu: Option<&str>, task_logs: &str) -> String {
    let config = app.global();
    let state = ProjectState::load(&app.project_id).unwrap_or_default();
    let mut s = String::new();

    let _ = writeln!(s, "Project directory: {}", app.project_dir.display());

    let mut servers: Vec<&Server> = config.servers.values().collect();
    servers.sort_by_key(|a| server_label(a));
    let _ = writeln!(s, "\nServers registered in yunpao ({}):", servers.len());
    for srv in &servers {
        let default = if config.default_server.as_deref() == Some(srv.id.as_str()) { " [default]" } else { "" };
        let _ = writeln!(s, "- {}: {}@{}:{}{default}", server_label(srv), srv.user, srv.host, srv.port);
    }

    // Alias names only: addresses of unrelated hosts shouldn't be sent to the LLM.
    let aliases = ssh_config::list_aliases();
    let _ = writeln!(s, "\nHost aliases in the user's ~/.ssh/config ({}):", aliases.len());
    for alias in &aliases {
        let imported = config
            .servers
            .values()
            .any(|srv| &srv.host == alias || srv.aliases.contains(alias));
        let added = if imported { "already in yunpao" } else { "NOT yet in yunpao — the user can add it with the「+ 从 SSH 导入」button above the server list" };
        let _ = writeln!(s, "- {alias} ({added})");
    }

    let current = state.current_session().map(|x| x.id.clone());
    let sessions: Vec<_> = state.list_sessions().collect();
    let _ = writeln!(s, "\nSessions ({}):", sessions.len());
    for sess in &sessions {
        let srv = config.servers.get(&sess.server)
            .map(server_label)
            .unwrap_or_else(|| sess.server.clone());
        let mark = if current.as_deref() == Some(sess.id.as_str()) { " [active]" } else { "" };
        let tasks = if sess.tasks.is_empty() {
            "no running tasks".to_string()
        } else {
            sess.tasks.iter().map(|(n, p)| format!("{n} (pid {p})")).collect::<Vec<_>>().join(", ")
        };
        let _ = writeln!(s, "- {} on {srv}{mark}, created {}: {tasks}", sess.name, sess.created_at_str());
    }

    let _ = writeln!(s, "\nActions in yunpao.toml ({}):", app.actions.len());
    for (name, cmd) in &app.actions {
        let _ = writeln!(s, "- {name}: {cmd}");
    }
    let scripts = jobs::project_scripts(&app.project_dir);
    let _ = writeln!(s, "\nRunnable files in the project folder ({}): {}", scripts.len(), scripts.join(", "));

    match gpu.filter(|g| !g.trim().is_empty()) {
        Some(g) => { let _ = writeln!(s, "\nLive GPU stats:\n{g}"); }
        None => { let _ = writeln!(s, "\nLive GPU stats: not available (no server selected or no NVIDIA GPU)"); }
    }
    if !task_logs.trim().is_empty() {
        let _ = writeln!(s, "\nLatest log lines of each task (newest last):\n{task_logs}");
    }
    s
}

// ── Streaming responses ──────────────────────────────────────────────────────

pub(super) type SseStream = Sse<std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>;

/// Server-sent events fed from a channel: handlers send `ev(...)` from wherever the work runs.
pub(super) fn sse_from(mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>) -> SseStream {
    let s = async_stream::stream! {
        while let Some(e) = rx.recv().await {
            yield Ok::<Event, Infallible>(e);
        }
    };
    Sse::new(Box::pin(s))
}

pub(super) fn ev(kind: &str, data: serde_json::Value) -> Event {
    Event::default().event(kind).data(data.to_string())
}

/// Events: `delta` {text} while the model writes, then `done` (ChatResp) or `error` {message}.
async fn api_chat(State(app): State<Arc<AppState>>, Json(req): Json<ChatReq>) -> SseStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let logs = recent_task_logs(&app).await;
        let tx2 = tx.clone();
        let result = tokio::task::spawn_blocking(move || {
            let snapshot = build_snapshot(&app, req.context.as_deref(), &logs);
            let mut on_delta = |d: &str| {
                let _ = tx2.send(ev("delta", serde_json::json!({ "text": d })));
            };
            ai::chat(&snapshot, &req.history, &req.message, &mut on_delta)
        })
        .await;
        let last = match result {
            Ok(Ok(r)) => ev("done", serde_json::json!(ChatResp { reply: r.reply, command: r.command, job: r.job })),
            Ok(Err(e)) => ev("error", serde_json::json!({ "message": format!("{e:#}") })),
            Err(e) => ev("error", serde_json::json!({ "message": e.to_string() })),
        };
        let _ = tx.send(last);
    });
    sse_from(rx)
}

/// The last lines of each task's log (one ssh per server), so the AI can answer
/// "how far is my training / is the loss going down".
async fn recent_task_logs(app: &AppState) -> String {
    let config = app.global();
    let state = ProjectState::load(&app.project_id).unwrap_or_default();
    let mut by_server: std::collections::HashMap<String, Vec<(String, String, String)>> = Default::default();
    for s in state.list_sessions() {
        for name in s.tasks.keys().filter(|n| is_safe_token(n)) {
            by_server.entry(s.server.clone()).or_default().push((s.id.clone(), s.name.clone(), name.clone()));
        }
    }
    let mut out = String::new();
    for (server_id, tasks) in by_server {
        let Some(server) = config.servers.get(&server_id) else { continue };
        let script: String = tasks
            .iter()
            .take(8)
            .map(|(sid, sname, name)| {
                format!(
                    "echo '### task {name} (session {}, server {})'; tail -n 12 {} 2>/dev/null | cut -c1-200;",
                    sname.replace(['\'', '\n'], " "),
                    server_label(server).replace(['\'', '\n'], " "),
                    remote_log_path(server, sid, name)
                )
            })
            .collect();
        match ssh_exec(server, &script).await {
            Ok((_, s)) => out.push_str(&s),
            Err(e) => {
                let _ = writeln!(out, "### server {} unreachable: {e}", server_label(server));
            }
        }
    }
    out
}

async fn api_logs(
    State(app): State<Arc<AppState>>,
    Query(q): Query<LogsQuery>,
) -> Json<LogsResp> {
    // All of these end up inside a remote shell command line.
    if !is_safe_token(&q.action) || !is_safe_token(&q.session_id) || q.run.as_deref().is_some_and(|r| !is_safe_token(r)) {
        return Json(LogsResp { lines: vec!["Invalid action or session name".into()] });
    }
    let state = match ProjectState::load(&app.project_id) {
        Ok(s) => s,
        Err(e) => return Json(LogsResp { lines: vec![e.to_string()] }),
    };
    // Runs synced from the servers may belong to another project or computer: their server
    // is known from the history.
    let server_id = match state.list_sessions().find(|s| s.id == q.session_id) {
        Some(s) => s.server.clone(),
        None => match jobs::server_of_session(&app.project_id, &q.session_id) {
            Some(id) => id,
            None => return Json(LogsResp { lines: vec!["Session not found".into()] }),
        },
    };
    let server = match app.global().servers.get(&server_id) {
        Some(s) => s.clone(),
        None => return Json(LogsResp { lines: vec!["Server not found".into()] }),
    };

    let log_path = match &q.run {
        Some(run) => remote_run_log_path(&server, &q.session_id, &q.action, run),
        None => remote_log_path(&server, &q.session_id, &q.action),
    };
    Json(LogsResp { lines: tail_remote(&server, &log_path, 100).await })
}

/// Must match where `RemoteContext` (src/ssh.rs) writes task logs. This is the latest run
/// (a symlink); each run's own log is at remote_run_log_path.
fn remote_log_path(server: &Server, session_id: &str, action: &str) -> String {
    let home = crate::ssh::remote_home(server);
    format!("{home}/.cache/yunpao/sessions/{session_id}/logs/{action}.log")
}

fn remote_run_log_path(server: &Server, session_id: &str, action: &str, run: &str) -> String {
    let home = crate::ssh::remote_home(server);
    format!("{home}/.cache/yunpao/sessions/{session_id}/logs/runs/{action}-{run}.log")
}

fn find_imported<'a>(config: &'a Global, h: &SshHost) -> Option<&'a Server> {
    config.servers.values().find(|s| {
        s.host == h.alias
            || s.aliases.iter().any(|a| a == &h.alias)
            || (s.host == h.hostname && s.port == h.port && s.user == h.user)
    })
}

async fn api_ssh_hosts(State(app): State<Arc<AppState>>) -> Json<Vec<SshHostInfo>> {
    let hosts = tokio::task::spawn_blocking(move || {
        let config = app.global();
        ssh_config::list_hosts()
            .into_iter()
            .map(|h| SshHostInfo { imported: find_imported(&config, &h).is_some(), host: h })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(hosts)
}

async fn api_ssh_import(Json(req): Json<ImportReq>) -> Json<ImportResp> {
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        // Only aliases that really exist in ~/.ssh/config are accepted.
        let known = ssh_config::list_aliases();
        // Must be a fresh load: saving the startup snapshot would clobber later edits.
        let mut config = Global::load().context("Could not read config.toml")?;
        let mut added = Vec::new();
        for alias in req.aliases.iter().filter(|a| known.contains(a)) {
            let Some(h) = ssh_config::resolve(alias) else { continue };
            if find_imported(&config, &h).is_some() {
                continue;
            }
            // host = the alias, so ssh keeps applying the user's config (keys, ProxyJump…).
            let port = h.port.to_string();
            let mut server = Server::new(
                HostArgs::FromData { user: &h.user, host: &h.alias, port: &port },
                None,
            )?;
            server.aliases.push(h.alias.clone());
            let id = server.id.clone();
            config.add_server(server)?;
            if config.default_server.is_none() {
                config.set_default_server(&id)?;
            }
            added.push(h.alias);
        }
        Ok(added)
    })
    .await;

    match result {
        Ok(Ok(added)) => Json(ImportResp { ok: true, added, message: String::new() }),
        Ok(Err(e)) => Json(ImportResp { ok: false, added: vec![], message: format!("{e:#}") }),
        Err(e) => Json(ImportResp { ok: false, added: vec![], message: e.to_string() }),
    }
}

fn settings_view() -> SettingsResp {
    let saved = AiSettings::load();
    let r = saved.resolve();
    SettingsResp {
        base_url: r.base_url,
        model: r.model,
        key_hint: r.api_key.as_deref().map(mask_key),
        key_source: r.key_source,
        path: AiSettings::path().map(|p| p.display().to_string()).unwrap_or_default(),
    }
}

fn mask_key(k: &str) -> String {
    let chars: Vec<char> = k.chars().collect();
    if chars.len() <= 10 {
        return "•".repeat(chars.len());
    }
    let head: String = chars[..5].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

async fn api_settings_get() -> Json<SettingsResp> {
    Json(settings_view())
}

async fn api_settings_save(Json(req): Json<SettingsReq>) -> Json<TestResp> {
    let mut s = AiSettings::load();
    let new_url = req.base_url.trim().trim_end_matches('/').to_string();
    if !req.api_key.trim().is_empty() {
        s.api_key = req.api_key.trim().to_string();
    } else if s.api_key.trim().is_empty() || new_url != s.resolve().base_url {
        // Keep an old key only for the same endpoint, never carry it to another provider.
        return Json(TestResp { ok: false, message: "换了服务商需要重新粘贴对应的 API Key".into() });
    }
    s.base_url = new_url;
    s.model = req.model.trim().to_string();
    match s.save() {
        Ok(()) => Json(TestResp { ok: true, message: "saved".into() }),
        Err(e) => Json(TestResp { ok: false, message: e.to_string() }),
    }
}

/// Key and endpoint for a test/model-list request. The saved key is reused only
/// against the endpoint it was saved with, never sent to another provider.
fn request_credentials(req: &SettingsReq) -> Option<(String, String)> {
    let current = AiSettings::load().resolve();
    let base_url = Some(req.base_url.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(current.base_url.clone());
    let key = Some(req.api_key.trim().to_string())
        .filter(|k| !k.is_empty())
        .or(current.api_key.filter(|_| base_url == current.base_url))?;
    Some((key, base_url))
}

#[derive(Serialize)]
struct ModelsResp {
    ok: bool,
    models: Vec<String>,
    message: String,
}

async fn api_settings_models(Json(req): Json<SettingsReq>) -> Json<ModelsResp> {
    let Some((key, base_url)) = request_credentials(&req) else {
        return Json(ModelsResp { ok: false, models: vec![], message: "no key".into() });
    };
    match tokio::task::spawn_blocking(move || ai::list_models(&key, &base_url)).await {
        Ok(Ok(models)) => Json(ModelsResp { ok: true, models, message: String::new() }),
        Ok(Err(e)) => Json(ModelsResp { ok: false, models: vec![], message: format!("{e:#}") }),
        Err(e) => Json(ModelsResp { ok: false, models: vec![], message: e.to_string() }),
    }
}

async fn api_settings_test(Json(req): Json<SettingsReq>) -> Json<TestResp> {
    let Some((key, base_url)) = request_credentials(&req) else {
        return Json(TestResp { ok: false, message: "请粘贴这个服务商的 API Key".into() });
    };
    let current = AiSettings::load().resolve();
    let model = Some(req.model.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(current.model);

    let result =
        tokio::task::spawn_blocking(move || ai::test_connection(&key, &base_url, &model)).await;
    match result {
        Ok(Ok(())) => Json(TestResp { ok: true, message: "ok".into() }),
        Ok(Err(e)) => Json(TestResp { ok: false, message: format!("{e:#}") }),
        Err(e) => Json(TestResp { ok: false, message: e.to_string() }),
    }
}

/// Reject requests whose Host isn't our own localhost address (DNS-rebinding guard).
async fn host_guard(State(app): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let ok = [format!("localhost:{}", app.port), format!("127.0.0.1:{}", app.port)]
        .iter()
        .any(|h| h == host);
    // A custom header can't be sent cross-site without a CORS preflight (which we never
    // approve), so other web pages can't trigger uploads, jobs or settings changes.
    let mutating = req.method() != axum::http::Method::GET;
    let marked = req.headers().get("x-yunpao").is_some_and(|v| v == "1");
    if ok && (!mutating || marked) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "forbidden host").into_response()
    }
}

// ── SSH helpers ───────────────────────────────────────────────────────────────

fn ssh_base_args(srv: &Server) -> Vec<String> {
    // No StrictHostKeyChecking override: command-line options beat ~/.ssh/config,
    // so forcing accept-new would silently weaken the user's own policy.
    let mut a = vec![
        "-o".into(), "ConnectTimeout=10".into(),
        "-o".into(), "BatchMode=yes".into(),
        "-p".into(), srv.port.to_string(),
    ];
    a.extend(crate::ssh::mux_opts(&srv.host));
    if let Some(id) = &srv.identity {
        a.push("-i".into());
        a.push(shellexpand::tilde(id).into_owned());
    }
    a.push(format!("{}@{}", srv.user, srv.host));
    a
}

/// Run a remote command. Err only when ssh itself failed (exit 255: unreachable,
/// auth refused…); a failing remote command still returns Ok with its stdout.
async fn ssh_exec(srv: &Server, cmd: &str) -> std::result::Result<(bool, String), String> {
    let mut args = ssh_base_args(srv);
    args.push(cmd.to_string());
    let out = Cmd::new("ssh").args(&args).output().await.map_err(|e| e.to_string())?;
    if out.status.code() == Some(255) {
        let err = String::from_utf8_lossy(&out.stderr);
        let reason = err
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with("**"))
            .last()
            .unwrap_or("ssh connection failed")
            .trim()
            .to_string();
        return Err(reason);
    }
    Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

async fn ssh_run(srv: &Server, cmd: &str) -> Option<String> {
    match ssh_exec(srv, cmd).await {
        Ok((true, out)) => Some(out),
        _ => None,
    }
}

async fn gpu_stats(srv: &Server) -> std::result::Result<Vec<GpuInfo>, String> {
    const SEP: &str = "===YUNPAO_PMON===";
    const SEP2: &str = "===YUNPAO_PS===";
    // One ssh call: GPU stats, per-GPU processes, then owner / runtime / command of those pids.
    let query = format!(
        "nvidia-smi \
        --query-gpu=index,name,utilization.gpu,utilization.memory,\
        memory.used,memory.total,temperature.gpu,power.draw,power.limit \
        --format=csv,noheader,nounits 2>/dev/null; echo {SEP}; \
        pm=$(nvidia-smi pmon -c 1 -s m 2>/dev/null | awk 'NR>2 && $2!=\"-\" {{print}}'); echo \"$pm\"; echo {SEP2}; \
        p=$(echo \"$pm\" | awk '{{print $2}}' | grep -E '^[0-9]+$' | sort -u | paste -sd, -); \
        [ -n \"$p\" ] && ps -o pid=,user:32=,etimes=,args= -p \"$p\" 2>/dev/null | cut -c1-300"
    );

    let (_, out) = ssh_exec(srv, &query).await?;
    let (raw, rest) = out.split_once(SEP).unwrap_or((out.as_str(), ""));
    let (pmon_raw, ps_raw) = rest.split_once(SEP2).unwrap_or((rest, ""));
    if raw.trim().is_empty() {
        return Ok(vec![]);
    }
    // pid → (user, elapsed seconds, command line)
    let owners: std::collections::HashMap<u32, (String, Option<u64>, String)> = ps_raw
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid: u32 = it.next()?.parse().ok()?;
            let user = it.next()?.to_string();
            let elapsed = it.next().and_then(|e| e.parse().ok());
            let cmd: String = it.collect::<Vec<_>>().join(" ").chars().take(160).collect();
            Some((pid, (user, elapsed, cmd)))
        })
        .collect();

    let procs: Vec<(u32, GpuProc)> = pmon_raw
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            if p.len() < 4 { return None; }
            let gpu_idx: u32 = p[0].parse().ok()?;
            let pid: u32 = p[1].parse().ok()?;
            let mem_mib: u32 = p[3].parse().unwrap_or(0);
            let name = p.last().unwrap_or(&"?").to_string();
            let (user, elapsed_secs, cmd) = owners.get(&pid).cloned().unwrap_or_default();
            Some((gpu_idx, GpuProc { pid, name, mem_mib, user, elapsed_secs, cmd }))
        })
        .collect();

    Ok(raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let p: Vec<&str> = line.split(',').map(str::trim).collect();
            if p.len() < 9 { return None; }
            let idx: u32 = p[0].parse().unwrap_or(0);
            let mem_total = p[5].parse().unwrap_or(0);
            Some(GpuInfo {
                index: idx,
                name: p[1].to_string(),
                profile: gpu_db::profile(p[1], mem_total),
                util_gpu: p[2].parse().unwrap_or(0),
                util_mem: p[3].parse().unwrap_or(0),
                mem_used: p[4].parse().unwrap_or(0),
                mem_total,
                temp: p[6].parse().unwrap_or(0),
                power_draw: p[7].parse().unwrap_or(0.0),
                power_limit: p[8].parse().unwrap_or(0.0),
                procs: procs
                    .iter()
                    .filter(|(g, _)| *g == idx)
                    .map(|(_, pr)| pr.clone())
                    .collect(),
            })
        })
        .collect())
}

async fn tail_remote(srv: &Server, path: &str, n: usize) -> Vec<String> {
    let cmd = format!("tail -{n} {path} 2>/dev/null || echo '(log not found)'");
    ssh_run(srv, &cmd)
        .await
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}
