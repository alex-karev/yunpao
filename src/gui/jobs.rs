//! Submitting, watching and stopping jobs from the dashboard.

use anyhow::{Context, Result, bail, ensure};
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

use super::{AppState, SseStream, ev, is_safe_token, remote_log_path, sse_from, ssh_exec};
use crate::{
    ai,
    config::{Global, Project},
    ssh::{RemoteCommand, RemoteContext},
};

// ── Project info (what can be run) ────────────────────────────────────────────

#[derive(Serialize)]
pub struct ProjectInfo {
    dir: String,
    actions: Vec<(String, String)>,
    scripts: Vec<String>,
    has_gitignore: bool,
    /// Set when the project folder is the home directory (or above): submitting is refused.
    too_broad: Option<String>,
}

/// Top-level .py / .sh files a beginner is likely to want to run.
pub fn project_scripts(dir: &std::path::Path) -> Vec<String> {
    let mut scripts: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.') && (n.ends_with(".py") || n.ends_with(".sh")))
        .collect();
    scripts.sort();
    scripts
}

pub async fn api_project(State(app): State<Arc<AppState>>) -> Json<ProjectInfo> {
    let dir = app.project_dir.clone();
    Json(ProjectInfo {
        dir: dir.display().to_string(),
        actions: app.actions.clone(),
        scripts: project_scripts(&dir),
        has_gitignore: dir.join(".gitignore").exists(),
        too_broad: crate::ssh::too_broad_project_dir(&dir),
    })
}

// ── Submit ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
pub struct JobReq {
    pub(super) server_id: String,
    pub(super) command: String,
    pub(super) name: String,
    #[serde(default)]
    pub(super) gpus: Vec<u32>,
    /// Absolute path of a conda env / venv on the server (from /api/envs); None = system python.
    #[serde(default)]
    pub(super) python_env: Option<String>,
    /// Queue only: start once this many GPUs are free (`gpus` is then filled in by the queue).
    #[serde(default)]
    pub(super) wait_gpus: u32,
}

#[derive(Serialize, Default)]
pub struct JobResp {
    pub(super) ok: bool,
    /// "running" when a task with this name is still alive (two runs would share one log file).
    code: String,
    pub(super) message: String,
    pid: String,
    pub(super) session_id: String,
    name: String,
    run_id: String,
}

/// An env path goes into a remote shell line: allow only plain absolute paths.
pub(super) fn safe_env_path(p: &str) -> bool {
    p.starts_with('/')
        && p.len() < 512
        && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-' | '+'))
        && !p.split('/').any(|seg| seg == "..")
}

/// Events: `phase` {phase: "check"|"upload"|"start"}, `upload` {done, total, bytes_done,
/// bytes_total, file}, then `done` (JobResp).
pub async fn api_submit(State(app): State<Arc<AppState>>, Json(req): Json<JobReq>) -> SseStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let name = req.name.trim().to_string();
    let command = req.command.trim().to_string();
    if let Some(msg) = invalid_job(&req) {
        let _ = tx.send(ev("done", serde_json::json!(JobResp { code: "invalid".into(), message: msg.into(), ..Default::default() })));
        return sse_from(rx);
    }
    tokio::task::spawn_blocking(move || {
        let mut emit = |e| {
            let _ = tx.send(e);
        };
        let resp = submit(&app, &req, &name, &command, &mut emit).unwrap_or_else(|e| JobResp {
            code: "failed".into(),
            message: format!("{e:#}"),
            ..Default::default()
        });
        emit(ev("done", serde_json::json!(resp)));
    });
    sse_from(rx)
}

pub(super) fn invalid_job(req: &JobReq) -> Option<&'static str> {
    if !is_safe_token(req.name.trim()) {
        Some("任务名只能用英文字母、数字、-、_、.")
    } else if req.command.trim().is_empty() {
        Some("请填写要运行的命令")
    } else if req.python_env.as_deref().is_some_and(|p| !safe_env_path(p)) {
        Some("Python 环境路径不合法")
    } else {
        None
    }
}

/// Submit attempts still uploading in this process (their temporary history ids).
static INFLIGHT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Blocking: [`submit_inner`], with every attempt recorded in the history — a submit that
/// fails (or is cut off) shows up there with its reason instead of silently vanishing.
pub(super) fn submit(app: &AppState, req: &JobReq, name: &str, command: &str, emit: &mut dyn FnMut(axum::response::sse::Event)) -> Result<JobResp> {
    let server_label = Global::load().ok().and_then(|c| c.servers.get(&req.server_id).map(super::server_label)).unwrap_or_default();
    let temp_id = format!("submit-{}", crate::utils::random_uuid());
    let mut entry = HistoryEntry {
        run_id: temp_id.clone(),
        time: chrono::Local::now().timestamp(),
        server_id: req.server_id.clone(),
        server_label,
        session_id: String::new(),
        name: name.to_string(),
        command: command.to_string(),
        gpus: req.gpus.clone(),
        python_env: req.python_env.clone(),
        pid: String::new(),
        outcome: "submitting".into(),
        error: String::new(),
        source: String::new(),
    };
    let _ = append_history(&app.project_id, entry.clone());
    INFLIGHT.lock().unwrap().push(temp_id.clone());

    let result = submit_inner(app, req, name, command, emit);
    match &result {
        Ok(r) if r.ok => {
            super::queue::reserve(&req.server_id, &req.gpus);
            entry.run_id = r.run_id.clone();
            entry.session_id = r.session_id.clone();
            entry.pid = r.pid.clone();
            entry.outcome = String::new();
        }
        Ok(r) => {
            entry.outcome = "failed".into();
            entry.error = r.message.clone();
        }
        Err(e) => {
            entry.outcome = "failed".into();
            entry.error = format!("{e:#}");
        }
    }
    let _ = replace_history(&app.project_id, &temp_id, entry);
    INFLIGHT.lock().unwrap().retain(|id| id != &temp_id);
    result
}

/// Records something that never became a run (e.g. a cancelled queue item).
pub(super) fn record_not_run(app: &AppState, req: &JobReq, server_label: &str, outcome: &str, error: &str) {
    let _ = append_history(&app.project_id, HistoryEntry {
        run_id: format!("{outcome}-{}", crate::utils::random_uuid()),
        time: chrono::Local::now().timestamp(),
        server_id: req.server_id.clone(),
        server_label: server_label.to_string(),
        session_id: String::new(),
        name: req.name.trim().to_string(),
        command: req.command.trim().to_string(),
        gpus: req.gpus.clone(),
        python_env: req.python_env.clone(),
        pid: String::new(),
        outcome: outcome.into(),
        error: error.into(),
        source: String::new(),
    });
}

/// A run started from the terminal (`yunpao run`), so it shows up in the page's history too.
#[allow(clippy::too_many_arguments)]
pub fn record_cli_run(project_id: &str, server_id: &str, server_label: &str, session_id: &str, name: &str, command: &str, pid: &str, run_id: &str) {
    let _ = append_history(project_id, HistoryEntry {
        run_id: run_id.into(),
        time: chrono::Local::now().timestamp(),
        server_id: server_id.into(),
        server_label: server_label.into(),
        session_id: session_id.into(),
        name: name.into(),
        command: command.into(),
        gpus: Vec::new(),
        python_env: None,
        pid: pid.into(),
        outcome: String::new(),
        error: String::new(),
        source: "cli".into(),
    });
}

fn submit_inner(app: &AppState, req: &JobReq, name: &str, command: &str, emit: &mut dyn FnMut(axum::response::sse::Event)) -> Result<JobResp> {
    let server_id = req.server_id.as_str();
    emit(ev("phase", serde_json::json!({ "phase": "check" })));
    if let Some(why) = crate::ssh::too_broad_project_dir(&app.project_dir) {
        bail!("{why}");
    }
    let config = Global::load().context("Could not read config.toml")?;
    ensure!(config.servers.contains_key(server_id), "Server not found");
    // Try the connection first: once the upload starts, ssh's own error (refused, password
    // needed…) is buried under rsync's "unexpected end of file".
    if let Some(srv) = config.servers.get(server_id) {
        let out = std::process::Command::new("ssh").args(super::ssh_base_args(srv)).arg("true")
            .stdin(std::process::Stdio::null()).output().context("Could not run ssh")?;
        if out.status.code() == Some(255) {
            let err = String::from_utf8_lossy(&out.stderr);
            let reason = err.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("**")).last().unwrap_or("ssh failed");
            bail!("连不上服务器 {}：{}", super::server_label(srv), reason.trim());
        }
    }
    let mut project = Project::load().context("Could not read yunpao.toml")?;

    // Use (or create) a session on the chosen server and make it the active one.
    let on_current = project.state.current_session().is_some_and(|s| s.server == server_id);
    if !on_current {
        let existing = project
            .state
            .list_sessions()
            .filter(|s| s.server == server_id)
            .max_by_key(|s| s.created_at)
            .map(|s| s.id.clone());
        match existing {
            Some(id) => project.state.switch_session(&id)?,
            None => project.state.new_session(&server_id.to_string(), None)?,
        }
    }
    let session_id = project.state.current_session().context("No active session")?.id.clone();

    let context = RemoteContext::new(&config, &project)?.batch_mode();
    if let Some(pid) = project.state.current_session().and_then(|s| s.tasks.get(name))
        && context.check_pid(pid)?
    {
        return Ok(JobResp {
            code: "running".into(),
            message: format!("「{name}」还在运行（PID {pid}）"),
            ..Default::default()
        });
    }

    // Upload with progress: plan first (dry run), then count files as rsync sends them.
    emit(ev("phase", serde_json::json!({ "phase": "upload" })));
    let plan = context.push_plan(false)?;
    let size_of = |p: &str| std::fs::metadata(app.project_dir.join(p)).map(|m| m.len()).unwrap_or(0);
    let sizes: HashMap<&str, u64> = plan.iter().map(|p| (p.as_str(), size_of(p))).collect();
    let (total, bytes_total) = (plan.len(), sizes.values().sum::<u64>());
    // Bytes = files already finished + how far rsync is into the current one.
    let (mut done, mut finished_bytes, mut current): (usize, u64, Option<(String, u64)>) = (0, 0, None);
    let mut last_emit = std::time::Instant::now() - std::time::Duration::from_secs(1);
    emit(ev("upload", serde_json::json!({ "done": 0, "total": total, "bytes_done": 0, "bytes_total": bytes_total, "file": "" })));
    if total > 0 {
        context.push_with_progress(|line| {
            let mut partial = 0u64;
            if let Some(&size) = sizes.get(line) {
                // A new file starts; the previous one is complete even if we missed its 100% line.
                if let Some((_, prev)) = current.take() {
                    finished_bytes += prev;
                    done += 1;
                }
                current = Some((line.to_string(), size));
            } else if let Some(first) = line.split_whitespace().next()
                && line.contains('%')
                && let Ok(n) = first.replace(',', "").parse::<u64>()
            {
                partial = n;
                if line.contains("100%")
                    && let Some((_, size)) = current.take()
                {
                    finished_bytes += size;
                    done += 1;
                    partial = 0;
                }
            } else {
                return; // an rsync message, not progress
            }
            let bytes_done = (finished_bytes + partial).min(bytes_total);
            if last_emit.elapsed().as_millis() >= 150 || done == total {
                last_emit = std::time::Instant::now();
                let file = current.as_ref().map(|c| c.0.as_str()).unwrap_or("");
                emit(ev("upload", serde_json::json!({ "done": done.min(total), "total": total, "bytes_done": bytes_done, "bytes_total": bytes_total, "file": file })));
            }
        })?;
        emit(ev("upload", serde_json::json!({ "done": total, "total": total, "bytes_done": bytes_total, "bytes_total": bytes_total, "file": "" })));
    }

    // Re-read the state that run() will save: the upload can take minutes, and stops or other
    // submits made meanwhile must not be overwritten by a stale copy.
    emit(ev("phase", serde_json::json!({ "phase": "start" })));
    let mut project = Project::load()?;
    if project.state.current_session().map(|s| s.id.as_str()) != Some(session_id.as_str()) {
        project.state.switch_session(&session_id)?;
    }

    let mut full = String::new();
    if let Some(env) = &req.python_env {
        // Putting the env's bin first is enough for python/pip, and needs no `conda init`.
        full.push_str(&format!("export PATH={env}/bin:$PATH; "));
    }
    if !req.gpus.is_empty() {
        let list = req.gpus.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        // nvidia-smi numbers GPUs by PCI bus, CUDA by speed: on servers with mixed models the
        // same index can be a different card. Make CUDA use nvidia-smi's (= the page's) order.
        full.push_str(&format!("export CUDA_DEVICE_ORDER=PCI_BUS_ID; export CUDA_VISIBLE_DEVICES={list}; "));
    }
    full.push_str(command);
    let name_owned = name.to_string();
    let (pid, run_id) = context.run_tracked(
        &mut project,
        RemoteCommand::Named { name: &name_owned, command: &full },
        false,
        None,
    )?;


    Ok(JobResp { ok: true, pid, session_id, name: name_owned, run_id, ..Default::default() })
}

// ── Upload preview ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PreviewReq {
    server_id: String,
}

#[derive(Serialize, Default)]
pub struct PreviewResp {
    ok: bool,
    message: String,
    files: usize,
    bytes: u64,
    largest: Vec<(String, u64)>,
    /// Files in the upload that look like secrets (keys, .env, credentials).
    sensitive: Vec<String>,
    /// Set when the project folder is a catch-all like Downloads / Desktop.
    junk_dir: Option<String>,
}

/// What submitting now would upload to this server: file count, size, biggest files.
pub async fn api_upload_preview(State(app): State<Arc<AppState>>, Json(req): Json<PreviewReq>) -> Json<PreviewResp> {
    let dir = app.project_dir.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<PreviewResp> {
        if let Some(why) = crate::ssh::too_broad_project_dir(&dir) {
            bail!("{why}");
        }
        let config = Global::load()?;
        ensure!(config.servers.contains_key(&req.server_id), "Server not found");
        let project = Project::load()?;
        // Compare against the session a submit would reuse; with none, everything is new.
        let existing = project
            .state
            .list_sessions()
            .filter(|s| s.server == req.server_id)
            .max_by_key(|s| s.created_at)
            .cloned();
        let session = existing.clone().unwrap_or_else(|| crate::storage::Session::new(&req.server_id, None));
        let context = RemoteContext::for_session(&config, &project, &session)?.batch_mode();
        let plan = context.push_plan(existing.is_none())?;
        let mut sized: Vec<(String, u64)> = plan
            .into_iter()
            .map(|p| {
                let n = std::fs::metadata(app.project_dir.join(&p)).map(|m| m.len()).unwrap_or(0);
                (p, n)
            })
            .collect();
        sized.sort_by(|a, b| b.1.cmp(&a.1));
        let sensitive = sized.iter().map(|x| x.0.clone()).filter(|p| crate::ssh::looks_secret(p)).take(20).collect();
        Ok(PreviewResp {
            ok: true,
            message: String::new(),
            files: sized.len(),
            bytes: sized.iter().map(|x| x.1).sum(),
            largest: sized.into_iter().take(5).collect(),
            sensitive,
            junk_dir: crate::ssh::junk_project_dir(&app.project_dir),
        })
    })
    .await;
    match result {
        Ok(Ok(r)) => Json(r),
        Ok(Err(e)) => Json(PreviewResp { message: format!("{e:#}"), ..Default::default() }),
        Err(e) => Json(PreviewResp { message: e.to_string(), ..Default::default() }),
    }
}

// ── Python environments on a server ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct EnvsQuery {
    server_id: String,
}

#[derive(Serialize)]
pub struct PyEnv {
    /// Absolute path of the env; null for the system python.
    path: Option<String>,
    name: String,
    /// "conda" | "venv" | "system"
    kind: String,
    version: String,
}

#[derive(Serialize, Default)]
pub struct EnvsResp {
    ok: bool,
    message: String,
    envs: Vec<PyEnv>,
}

/// Conda envs and venvs found in the usual places. Doesn't need `conda` on PATH
/// (often it isn't): reads ~/.conda/environments.txt and scans the common directories.
pub async fn api_envs(
    State(app): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<EnvsQuery>,
) -> Json<EnvsResp> {
    let config = app.global();
    let Some(server) = config.servers.get(&q.server_id) else {
        return Json(EnvsResp { message: "Server not found".into(), ..Default::default() });
    };
    let script = r#"for d in $(cat ~/.conda/environments.txt 2>/dev/null) ~/.conda/envs/* ~/miniconda3 ~/miniconda3/envs/* ~/anaconda3 ~/anaconda3/envs/* ~/miniforge3 ~/miniforge3/envs/* /opt/*conda*/ /opt/*conda*/envs/* ~/venv ~/.venv ~/envs/* ~/.virtualenvs/*; do
  if [ -x "$d/bin/python" ]; then k=venv; [ -d "$d/conda-meta" ] && k=conda; echo "ENV|$(cd "$d" && pwd -P)|$k|$("$d/bin/python" --version 2>&1 | head -1)"; fi
done | sort -u
echo "SYS|$(python3 --version 2>&1 | head -1)""#;
    let out = match ssh_exec(server, script).await {
        Ok((_, s)) => s,
        Err(e) => return Json(EnvsResp { message: e, ..Default::default() }),
    };
    let mut envs = Vec::new();
    for line in out.lines() {
        let p: Vec<&str> = line.split('|').collect();
        match p.as_slice() {
            ["SYS", version] => envs.insert(0, PyEnv { path: None, name: "系统自带 Python".into(), kind: "system".into(), version: version.trim().into() }),
            ["ENV", path, kind, version] if safe_env_path(path) => {
                let base = path.rsplit('/').next().unwrap_or(path);
                let name = if *kind == "conda" && !path.contains("/envs/") { "base".to_string() } else { base.to_string() };
                if !envs.iter().any(|e: &PyEnv| e.path.as_deref() == Some(*path)) {
                    envs.push(PyEnv { path: Some(path.to_string()), name, kind: kind.to_string(), version: version.trim().into() });
                }
            }
            _ => {}
        }
    }
    Json(EnvsResp { ok: true, envs, ..Default::default() })
}

// ── History ──────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct HistoryEntry {
    run_id: String,
    time: i64,
    server_id: String,
    server_label: String,
    session_id: String,
    name: String,
    /// The command as the user typed it (without env / GPU prefixes).
    command: String,
    gpus: Vec<u32>,
    python_env: Option<String>,
    pid: String,
    /// "" = started; "submitting" (upload in progress); "failed" (never started); "cancelled".
    #[serde(default)]
    outcome: String,
    /// Why it failed, in the words shown to the user.
    #[serde(default)]
    error: String,
    /// Where the run came from: "" (web page), "cli" (`yunpao run`), "server" (found in the
    /// server's run logs by a sync: another terminal, another computer, older versions…).
    #[serde(default)]
    source: String,
}

/// A results download, shown under the run it came from.
#[derive(Serialize, Deserialize, Clone)]
pub struct PullRecord {
    time: i64,
    session_id: String,
    files: Vec<String>,
}

/// Records live in the data dir, not the cache dir: cleanup tools empty caches.
/// `kind` is "history" or "pulls". Files from older versions (cache dir) are moved over.
fn record_path(kind: &str, project_id: &str) -> Option<std::path::PathBuf> {
    let name = format!("{project_id}.json");
    let path = dirs::data_local_dir()?.join("yunpao").join(kind).join(&name);
    if !path.exists()
        && let Some(old) = dirs::cache_dir().map(|d| d.join("yunpao").join(kind).join(&name))
        && old.exists()
    {
        let _ = std::fs::create_dir_all(path.parent()?);
        let _ = std::fs::rename(&old, &path).or_else(|_| std::fs::copy(&old, &path).map(|_| ()));
    }
    Some(path)
}

fn load_records<T: serde::de::DeserializeOwned>(kind: &str, project_id: &str) -> Vec<T> {
    record_path(kind, project_id)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// One writer at a time for the record files (read-modify-write).
static RECORDS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn append_record<T: Serialize + serde::de::DeserializeOwned>(kind: &str, project_id: &str, entry: T, max: usize) -> Result<()> {
    let _g = RECORDS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = record_path(kind, project_id).context("no data dir")?;
    let mut all: Vec<T> = load_records(kind, project_id);
    all.push(entry);
    let keep = all.len().saturating_sub(max);
    all.drain(..keep);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write-then-rename so a crash never leaves a half-written file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&all)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn load_history(project_id: &str) -> Vec<HistoryEntry> {
    load_records("history", project_id)
}

const HISTORY_MAX: usize = 500;

/// The server a session ran on, from the history (for sessions that aren't this project's).
pub(super) fn server_of_session(project_id: &str, session_id: &str) -> Option<String> {
    load_history(project_id).into_iter().find(|e| e.session_id == session_id).map(|e| e.server_id)
}

fn append_history(project_id: &str, entry: HistoryEntry) -> Result<()> {
    append_record("history", project_id, entry, HISTORY_MAX)
}

/// Swaps the entry `run_id` for `entry` (a submit attempt that finished one way or another).
fn replace_history(project_id: &str, run_id: &str, entry: HistoryEntry) -> Result<()> {
    let _g = RECORDS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = record_path("history", project_id).context("no data dir")?;
    let mut all = load_history(project_id);
    match all.iter_mut().find(|e| e.run_id == run_id) {
        Some(slot) => *slot = entry,
        None => all.push(entry),
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&all)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[derive(Serialize)]
pub struct HistoryItem {
    #[serde(flatten)]
    entry: HistoryEntry,
    status: TaskStatus,
    /// Results downloaded after this run (and before the session's next one).
    pulls: Vec<PullRecord>,
    /// The run's session isn't one of this project's (another project or computer).
    foreign: bool,
}

/// Past submissions, newest first, with each run's state (one ssh per server).
pub async fn api_history(State(app): State<Arc<AppState>>) -> Json<Vec<HistoryItem>> {
    let config = app.global();
    let mut entries = load_history(&app.project_id);
    // Each download belongs to the latest run in its session that started before it.
    let mut pulls_of: HashMap<String, Vec<PullRecord>> = HashMap::new();
    for p in load_records::<PullRecord>("pulls", &app.project_id) {
        if let Some(e) = entries.iter().filter(|e| e.session_id == p.session_id && e.time <= p.time).max_by_key(|e| e.time) {
            pulls_of.entry(e.run_id.clone()).or_default().push(p);
        }
    }
    entries.reverse();
    entries.truncate(100);
    let own_sessions: std::collections::HashSet<String> = crate::storage::ProjectState::load(&app.project_id)
        .map(|st| st.list_sessions().map(|s| s.id.clone()).collect())
        .unwrap_or_default();

    let mut status: HashMap<String, TaskStatus> = HashMap::new();
    let mut by_server: HashMap<String, Vec<&HistoryEntry>> = HashMap::new();
    for e in &entries {
        if is_safe_token(&e.run_id) && is_safe_token(&e.name) && is_safe_token(&e.session_id) && e.pid.chars().all(|c| c.is_ascii_digit()) {
            by_server.entry(e.server_id.clone()).or_default().push(e);
        }
    }
    for (server_id, list) in by_server {
        let Some(server) = config.servers.get(&server_id) else { continue };
        // Runs synced from the server have no pid: they're running if a process still holds
        // their log open.
        let mut script = String::from("OPEN=$(find /proc/[0-9]*/fd -lname '*/logs/runs/*.log' -printf '%l\\n' 2>/dev/null);");
        script.extend(list.iter().map(|e| {
                let log = super::remote_run_log_path(server, &e.session_id, &e.name, &e.run_id);
                if e.pid.is_empty() {
                    return format!(
                        "if printf '%s\\n' \"$OPEN\" | grep -qF 'runs/{name}-{run}.log'; then echo '{run} running'; \
                         elif [ -f {log} ]; then echo \"{run} done $(grep -ao 'Exit code: [0-9]*' {log} | tail -1 | grep -o '[0-9]*$')\"; \
                         else echo '{run} gone'; fi;",
                        name = e.name,
                        run = e.run_id,
                    );
                }
                // Running only if the pid is alive AND this run's log is the one it writes to.
                format!(
                    "if {alive} && [ \"$(readlink {latest})\" = \"runs/{name}-{run}.log\" ]; then echo '{run} running'; \
                     elif [ -f {log} ]; then echo \"{run} done $(grep -ao 'Exit code: [0-9]*' {log} | tail -1 | grep -o '[0-9]*$')\"; \
                     else echo '{run} gone'; fi;",
                    alive = crate::ssh::task_alive_check(&e.pid),
                    latest = super::remote_log_path(server, &e.session_id, &e.name),
                    name = e.name,
                    run = e.run_id,
                )
            }));
        if let Ok((_, out)) = ssh_exec(server, &script).await {
            for line in out.lines() {
                let mut p = line.split_whitespace();
                if let (Some(run), Some(st)) = (p.next(), p.next()) {
                    status.insert(run.to_string(), TaskStatus { state: st.into(), exit_code: p.next().and_then(|c| c.parse().ok()) });
                }
            }
        }
    }
    Json(
        entries
            .into_iter()
            .map(|e| {
                let fixed = match e.outcome.as_str() {
                    "submitting" if INFLIGHT.lock().unwrap().contains(&e.run_id) => Some("submitting"),
                    "submitting" => Some("interrupted"), // yunpao was closed mid-upload
                    "failed" => Some("submit_failed"),
                    "cancelled" => Some("cancelled"),
                    _ => None,
                };
                let st = match fixed {
                    Some(state) => TaskStatus { state: state.into(), exit_code: None },
                    None => status.get(&e.run_id).cloned().unwrap_or(TaskStatus { state: "unknown".into(), exit_code: None }),
                };
                let pulls = pulls_of.remove(&e.run_id).unwrap_or_default();
                let foreign = !e.session_id.is_empty() && !own_sessions.contains(&e.session_id);
                HistoryItem { entry: e, status: st, pulls, foreign }
            })
            .collect(),
    )
}

// ── Sync from the servers ────────────────────────────────────────────────────

/// One run as read back from its log on the server.
#[derive(Debug, PartialEq)]
struct ServerRun {
    session_id: String,
    name: String,
    run_id: String,
    time: i64,
    command: String,
    python_env: Option<String>,
    gpus: Vec<u32>,
}

/// `.../sessions/<session>/logs/runs/<name>-<YYYYMMDD-HHMMSS-xxxx>.log` → (session, name, run id).
fn parse_run_path(path: &str) -> Option<(String, String, String)> {
    let (head, file) = path.rsplit_once("/logs/runs/")?;
    let session = head.rsplit('/').next()?.to_string();
    let stem = file.strip_suffix(".log")?;
    let parts: Vec<&str> = stem.rsplitn(4, '-').collect(); // [rand, hhmmss, yyyymmdd, name]
    if parts.len() != 4 || parts[2].len() != 8 || parts[1].len() != 6 || !parts[1..3].iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let run_id = format!("{}-{}-{}", parts[2], parts[1], parts[0]);
    let ok = |s: &str| is_safe_token(s);
    (ok(&session) && ok(parts[3]) && ok(&run_id)).then(|| (session, parts[3].to_string(), run_id))
}

/// The command as typed: yunpao's own `export PATH=<env>/bin:$PATH; ` and
/// `export CUDA_VISIBLE_DEVICES=…; ` prefixes become the env / GPU fields.
fn split_command(line: &str) -> (String, Option<String>, Vec<u32>) {
    let mut rest = line.trim();
    let (mut env, mut gpus) = (None, Vec::new());
    loop {
        if let Some(r) = rest.strip_prefix("export PATH=")
            && let Some((path, after)) = r.split_once("/bin:$PATH; ")
        {
            env = Some(path.to_string());
            rest = after;
        } else if let Some(r) = rest.strip_prefix("export CUDA_DEVICE_ORDER=PCI_BUS_ID; ") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("export CUDA_VISIBLE_DEVICES=")
            && let Some((list, after)) = r.split_once("; ")
        {
            gpus = list.split(',').filter_map(|g| g.trim().parse().ok()).collect();
            rest = after;
        } else {
            break;
        }
    }
    (rest.to_string(), env, gpus)
}

/// "2026-11-09 14:57:49 +0800" (newer logs carry the zone); older logs without it are read
/// in the server's zone (`server_tz`, from `date +%z`), else this computer's.
fn parse_log_time(t: &str, server_tz: Option<chrono::FixedOffset>) -> Option<i64> {
    let t = t.trim();
    if let Ok(d) = chrono::DateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S %z") {
        return Some(d.timestamp());
    }
    let naive = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S").ok()?;
    match server_tz {
        Some(tz) => naive.and_local_timezone(tz).single().map(|d| d.timestamp()),
        None => naive.and_local_timezone(chrono::Local).single().map(|d| d.timestamp()),
    }
}

fn parse_server_runs(out: &str) -> Vec<ServerRun> {
    let mut runs = Vec::new();
    let mut cur: Option<ServerRun> = None;
    let mut server_tz = None;
    for line in out.lines() {
        if let Some(z) = line.strip_prefix("TZ ") {
            server_tz = chrono::DateTime::parse_from_str(&format!("2000-01-01 00:00:00 {}", z.trim()), "%Y-%m-%d %H:%M:%S %z").ok().map(|d| *d.offset());
        } else if let Some(path) = line.strip_prefix("F ") {
            runs.extend(cur.take());
            cur = parse_run_path(path.trim()).map(|(session_id, name, run_id)| ServerRun {
                session_id, name, run_id, time: 0, command: String::new(), python_env: None, gpus: Vec::new(),
            });
        } else if let Some(r) = cur.as_mut() {
            if let Some(t) = line.strip_prefix("[yunpao] Started: ") {
                r.time = parse_log_time(t, server_tz).unwrap_or(0);
            } else if let Some(c) = line.strip_prefix("[yunpao] Command: ") {
                (r.command, r.python_env, r.gpus) = split_command(c);
            }
        }
    }
    runs.extend(cur);
    runs.retain(|r| r.time > 0);
    runs
}

#[derive(Serialize, Default)]
pub struct SyncResp {
    ok: bool,
    added: usize,
    /// Servers that couldn't be reached, as "label: reason".
    failed: Vec<String>,
}

/// Reads every run log on every server (any project, terminal, computer) and adds the runs
/// the history doesn't have yet.
pub async fn api_history_sync(State(app): State<Arc<AppState>>) -> Json<SyncResp> {
    const SCRIPT: &str = "echo \"TZ $(date +%z)\"; for f in $(ls -t ~/.cache/yunpao/sessions/*/logs/runs/*.log 2>/dev/null | head -300); do \
        echo \"F $f\"; head -c 4000 \"$f\" | grep -a -m2 -E '^\\[yunpao\\] (Started|Command):'; done";
    let config = app.global();
    let mut found: Vec<(String, String, ServerRun)> = Vec::new();
    let mut failed = Vec::new();
    for (id, server) in &config.servers {
        match ssh_exec(server, SCRIPT).await {
            Ok((_, out)) => found.extend(parse_server_runs(&out).into_iter().map(|r| (id.clone(), super::server_label(server), r))),
            Err(e) => failed.push(format!("{}: {e}", super::server_label(server))),
        }
    }
    let project_id = app.project_id.clone();
    let added = tokio::task::spawn_blocking(move || -> Result<usize> {
        let _g = RECORDS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = record_path("history", &project_id).context("no data dir")?;
        let mut all = load_history(&project_id);
        let known: std::collections::HashSet<String> = all.iter().map(|e| e.run_id.clone()).collect();
        let before = all.len();
        for (server_id, server_label, r) in found {
            if known.contains(&r.run_id) {
                continue;
            }
            all.push(HistoryEntry {
                run_id: r.run_id, time: r.time, server_id, server_label, session_id: r.session_id,
                name: r.name, command: r.command, gpus: r.gpus, python_env: r.python_env,
                pid: String::new(), outcome: String::new(), error: String::new(), source: "server".into(),
            });
        }
        let added = all.len() - before;
        if added > 0 {
            all.sort_by_key(|e| e.time);
            let keep = all.len().saturating_sub(HISTORY_MAX);
            all.drain(..keep);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(&all)?)?;
            std::fs::rename(tmp, path)?;
        }
        Ok(added)
    })
    .await;
    match added {
        Ok(Ok(added)) => Json(SyncResp { ok: true, added, failed }),
        _ => Json(SyncResp { ok: false, added: 0, failed }),
    }
}

// ── Status ───────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct TaskStatus {
    /// "running" | "done" | "unknown"
    state: String,
    exit_code: Option<i32>,
}

/// session_id → task name → status. One ssh call per server.
pub async fn api_status(State(app): State<Arc<AppState>>) -> Json<HashMap<String, HashMap<String, TaskStatus>>> {
    let config = app.global();
    let state = crate::storage::ProjectState::load(&app.project_id).unwrap_or_default();

    let mut by_server: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    for s in state.list_sessions() {
        for (name, pid) in &s.tasks {
            if is_safe_token(name) && !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) {
                by_server.entry(s.server.clone()).or_default().push((s.id.clone(), name.clone(), pid.clone()));
            }
        }
    }

    let mut out: HashMap<String, HashMap<String, TaskStatus>> = HashMap::new();
    for (server_id, tasks) in by_server {
        let Some(server) = config.servers.get(&server_id) else { continue };
        let script: String = tasks
            .iter()
            .map(|(sid, name, pid)| {
                let log = remote_log_path(server, sid, name);
                format!(
                    "if {alive}; then echo '{sid} {name} running'; \
                     else echo \"{sid} {name} done $(grep -ao 'Exit code: [0-9]*' {log} 2>/dev/null | tail -1 | grep -o '[0-9]*$')\"; fi;",
                    alive = crate::ssh::task_alive_check(pid)
                )
            })
            .collect();
        let lines = match ssh_exec(server, &script).await {
            Ok((_, s)) => s,
            Err(_) => {
                for (sid, name, _) in tasks {
                    out.entry(sid).or_default().insert(name, TaskStatus { state: "unknown".into(), exit_code: None });
                }
                continue;
            }
        };
        for line in lines.lines() {
            let mut p = line.split_whitespace();
            let (Some(sid), Some(name), Some(st)) = (p.next(), p.next(), p.next()) else { continue };
            let exit_code = p.next().and_then(|c| c.parse().ok());
            out.entry(sid.into()).or_default().insert(name.into(), TaskStatus { state: st.into(), exit_code });
        }
    }
    Json(out)
}

// ── Stop / remove ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StopReq {
    session_id: String,
    name: String,
}

#[derive(Serialize)]
pub struct StopResp {
    ok: bool,
    message: String,
}

/// Stop the task if it's still running, then drop it from the task list.
pub async fn api_stop(State(app): State<Arc<AppState>>, Json(req): Json<StopReq>) -> Json<StopResp> {
    match stop(&app, &req).await {
        Ok(message) => Json(StopResp { ok: true, message }),
        Err(e) => Json(StopResp { ok: false, message: format!("{e:#}") }),
    }
}

async fn stop(app: &AppState, req: &StopReq) -> Result<String> {
    ensure!(is_safe_token(&req.session_id) && is_safe_token(&req.name), "Invalid task");
    let config = app.global();
    let state = crate::storage::ProjectState::load(&app.project_id)?;
    let session = state.list_sessions().find(|s| s.id == req.session_id).context("Session not found")?;
    let Some(pid) = session.tasks.get(&req.name).cloned() else { bail!("Task not found") };
    ensure!(pid.chars().all(|c| c.is_ascii_digit()), "Invalid pid");
    let server = config.servers.get(&session.server).context("Server not found")?;

    // The task is a bash wrapper around the real program (python…): stop the whole tree,
    // otherwise the training keeps running and holding the GPU. Only if the pid is still ours.
    // TERM first; if anything in the tree is still alive after 5 s (stuck CUDA call, DataLoader
    // workers…), KILL it; report honestly if even that fails instead of dropping it from the list.
    let script = format!(
        "if {alive}; then \
           L=\"{pid} $(pgrep -g {pid}) $(pgrep -P {pid}) $(for k in $(pgrep -P {pid}); do pgrep -P $k; done)\"; \
           up() {{ for p in $L; do kill -0 $p 2>/dev/null && return 0; done; return 1; }}; \
           kill -TERM -- -{pid} 2>/dev/null; kill -TERM $L 2>/dev/null; \
           for i in 1 2 3 4 5; do up || break; sleep 1; done; \
           if up; then kill -KILL -- -{pid} 2>/dev/null; kill -KILL $L 2>/dev/null; sleep 1; \
             if up; then echo stuck; else echo killed; fi; else echo stopped; fi; \
         else echo gone; fi",
        alive = crate::ssh::task_alive_check(&pid),
    );
    let (_, out) = ssh_exec(server, &script).await.map_err(|e| anyhow::anyhow!(e))?;
    if out.contains("stuck") {
        bail!("进程没能停下来（可能卡在显卡调用里）。等一会再点一次「停止」，还不行就请管理员处理；它还在占着卡。");
    }
    // Re-read under the lock: a submit may have saved the state while ssh was running.
    crate::storage::ProjectState::update(&app.project_id, |state| {
        if let Some(s) = state.session_mut(&req.session_id)
            && s.tasks.get(&req.name) == Some(&pid)
        {
            s.tasks.remove(&req.name);
        }
        Ok(())
    })?;
    Ok(if out.contains("killed") { "已强制停止（程序没响应正常的停止信号）".into() } else if out.contains("stopped") { "已停止".into() } else { "已从列表移除".into() })
}

// ── AI diagnosis ─────────────────────────────────────────────────────────────

pub(super) fn remote_work_path(server: &crate::config::Server, session_id: &str) -> String {
    let home = crate::ssh::remote_home(server);
    format!("{home}/.cache/yunpao/sessions/{session_id}/working")
}

#[derive(Deserialize)]
pub struct DiagnoseReq {
    session_id: String,
    name: String,
    /// Diagnose an earlier run instead of the latest.
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    context: String,
}

#[derive(Serialize, Default)]
pub struct DiagnoseResp {
    ok: bool,
    message: String,
    exit_code: Option<i32>,
    command: String,
    diagnosis: Option<ai::Diagnosis>,
}

/// Events: `delta` {text} while the model writes, then `done` (DiagnoseResp).
pub async fn api_diagnose(State(app): State<Arc<AppState>>, Json(req): Json<DiagnoseReq>) -> SseStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let resp = match diagnose(&app, req, tx.clone()).await {
            Ok(r) => r,
            Err(e) => DiagnoseResp { message: format!("{e:#}"), ..Default::default() },
        };
        let _ = tx.send(ev("done", serde_json::json!(resp)));
    });
    sse_from(rx)
}

/// `File "/home/u/.cache/yunpao/sessions/<id>/working/train.py", line 12` → (train.py, 12),
/// for frames inside the project (not site-packages), in traceback order.
fn traceback_frames(log: &str, session_id: &str) -> Vec<(String, usize)> {
    // Match on the session's working dir rather than the full home path: Python prints the
    // real path, which differs when the home directory is a symlink.
    let marker = format!("/sessions/{session_id}/working/");
    let mut frames: Vec<(String, usize)> = Vec::new();
    for line in log.lines() {
        let Some(rest) = line.trim_start().strip_prefix("File \"") else { continue };
        let Some((path, after)) = rest.split_once('"') else { continue };
        let Some(num) = after.trim_start_matches([',', ' ']).strip_prefix("line ") else { continue };
        let Ok(n) = num.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("").parse::<usize>() else { continue };
        let rel = match path.find(&marker) {
            Some(i) => &path[i + marker.len()..],
            None if !path.starts_with('/') && !path.starts_with('<') => path.trim_start_matches("./"),
            None => continue,
        };
        if safe_rel(rel) && !frames.iter().any(|f| f.0 == rel && f.1 == n) {
            frames.push((rel.to_string(), n));
        }
    }
    frames
}

/// Numbered lines around the last two frames whose file exists in the local project folder.
fn source_snippets(project_dir: &std::path::Path, frames: &[(String, usize)]) -> String {
    let mut out = String::new();
    let picked: Vec<&(String, usize)> = frames.iter().rev().filter(|(rel, _)| project_dir.join(rel).is_file()).take(2).collect();
    for (rel, line) in picked {
        let Ok(text) = std::fs::read_to_string(project_dir.join(rel)) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        let (from, to) = (line.saturating_sub(7), (*line + 3).min(lines.len()));
        out.push_str(&format!("{rel}:\n"));
        for (i, l) in lines.iter().enumerate().take(to).skip(from) {
            let mark = if i + 1 == *line { ">>" } else { "  " };
            out.push_str(&format!("{mark}{:>5} | {}\n", i + 1, l.chars().take(160).collect::<String>()));
        }
    }
    out.chars().take(4000).collect()
}

async fn diagnose(
    app: &AppState,
    req: DiagnoseReq,
    tx: tokio::sync::mpsc::UnboundedSender<axum::response::sse::Event>,
) -> Result<DiagnoseResp> {
    ensure!(is_safe_token(&req.session_id) && is_safe_token(&req.name), "Invalid task");
    ensure!(req.run.as_deref().is_none_or(is_safe_token), "Invalid run");
    let config = app.global();
    let state = crate::storage::ProjectState::load(&app.project_id)?;
    let session = state.list_sessions().find(|s| s.id == req.session_id).context("Session not found")?;
    let server = config.servers.get(&session.server).context("Server not found")?.clone();
    let log = match &req.run {
        Some(run) => super::remote_run_log_path(&server, &req.session_id, &req.name, run),
        None => remote_log_path(&server, &req.session_id, &req.name),
    };

    const SEP: &str = "===YUNPAO_SEP===";
    let script = format!(
        "head -3 {log} 2>/dev/null; echo {SEP}; tail -80 {log} 2>/dev/null; echo {SEP}; \
         grep -ao 'Exit code: [0-9]*' {log} 2>/dev/null | tail -1 | grep -o '[0-9]*$'"
    );
    let (_, out) = ssh_exec(&server, &script).await.map_err(|e| anyhow::anyhow!(e))?;
    let mut parts = out.splitn(3, SEP);
    let head = parts.next().unwrap_or("");
    let tail = parts.next().unwrap_or("").trim().to_string();
    let exit_code = parts.next().and_then(|s| s.trim().parse().ok());
    ensure!(!tail.is_empty(), "服务器上找不到这个任务的日志");
    let command = head
        .lines()
        .find_map(|l| l.strip_prefix("[yunpao] Command: "))
        .unwrap_or("")
        .to_string();
    let source = source_snippets(&app.project_dir, &traceback_frames(&tail, &req.session_id));

    let (cmd, ctx) = (command.clone(), req.context);
    let diagnosis = tokio::task::spawn_blocking(move || {
        let mut on_delta = |d: &str| {
            let _ = tx.send(ev("delta", serde_json::json!({ "text": d })));
        };
        ai::diagnose(&cmd, exit_code, &tail, &source, &ctx, &mut on_delta)
    })
    .await??;
    Ok(DiagnoseResp { ok: true, message: String::new(), exit_code, command, diagnosis: Some(diagnosis) })
}

// ── Results: list, pull, preview ─────────────────────────────────────────────

/// A relative path that stays inside the folder it is joined to.
pub(super) fn safe_rel(p: &str) -> bool {
    !p.is_empty()
        && p.len() < 1024
        && !p.starts_with('/')
        && !p.contains(['\\', '\0', '\n'])
        && std::path::Path::new(p).components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

#[derive(Deserialize)]
pub struct ResultsQuery {
    session_id: String,
}

#[derive(Serialize)]
pub struct RemoteFile {
    path: String,
    size: u64,
    mtime: f64,
    /// "new" (not on this computer), "changed" (server copy is newer), "local_newer"
    /// (edited here since the upload) or "same".
    state: String,
    /// Written on the server after this session's latest run started, i.e. by that job — not an
    /// uploaded file (uploads keep the local mtime), a stale file deleted here, or a dataset
    /// copied in by hand long before. Only these are pre-selected for download.
    produced: bool,
}

#[derive(Serialize, Default)]
pub struct ResultsResp {
    ok: bool,
    message: String,
    files: Vec<RemoteFile>,
}

pub async fn api_results(
    State(app): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<ResultsQuery>,
) -> Json<ResultsResp> {
    match list_results(&app, &q.session_id).await {
        Ok(files) => Json(ResultsResp { ok: true, files, ..Default::default() }),
        Err(e) => Json(ResultsResp { message: format!("{e:#}"), ..Default::default() }),
    }
}

async fn list_results(app: &AppState, session_id: &str) -> Result<Vec<RemoteFile>> {
    ensure!(is_safe_token(session_id), "Invalid session");
    let config = app.global();
    let state = crate::storage::ProjectState::load(&app.project_id)?;
    let session = state.list_sessions().find(|s| s.id == session_id).context("Session not found")?;
    let server = config.servers.get(&session.server).context("Server not found")?.clone();
    let work = remote_work_path(&server, session_id);
    // Uploads happen before a run starts and keep local mtimes, so anything newer than the
    // latest run's start was written by that run.
    let last_run = load_history(&app.project_id)
        .iter()
        .filter(|e| e.session_id == session_id && e.outcome.is_empty())
        .map(|e| e.time)
        .max()
        .unwrap_or(session.created_at as i64);
    // Python caches and notebook checkpoints are never results.
    let script = format!(
        "cd {work} 2>/dev/null && find . -type f -not -path './.git/*' -not -path '*/__pycache__/*' -not -name '*.pyc' \
         -not -path '*/.ipynb_checkpoints/*' -printf '%T@\\t%s\\t%P\\n' | sort -rn | head -2000"
    );
    let (_, out) = ssh_exec(&server, &script).await.map_err(|e| anyhow::anyhow!(e))?;
    let files = out
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            let mtime: f64 = p.next()?.parse().ok()?;
            let size: u64 = p.next()?.parse().ok()?;
            let path = p.next()?.to_string();
            if !safe_rel(&path) {
                return None;
            }
            let local = app.project_dir.join(&path);
            let state = match std::fs::metadata(&local) {
                Err(_) => "new",
                Ok(m) => {
                    let lm = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    // Uploads keep mtimes, so a newer remote file means the job wrote it.
                    // A differing file that is newer HERE was edited locally: never offer to
                    // overwrite it with the server's older copy.
                    if mtime > lm + 2.0 {
                        "changed"
                    } else if m.len() != size {
                        "local_newer"
                    } else {
                        "same"
                    }
                }
            };
            // A minute of slack for clock differences between this computer and the server.
            let produced = mtime >= (last_run - 60) as f64;
            Some(RemoteFile { path, size, mtime, state: state.into(), produced })
        })
        .collect();
    Ok(files)
}

#[derive(Deserialize)]
pub struct PullReq {
    session_id: String,
    paths: Vec<String>,
}

#[derive(Serialize, Default)]
pub struct PullResp {
    ok: bool,
    message: String,
    pulled: Vec<String>,
}

pub async fn api_pull(State(app): State<Arc<AppState>>, Json(req): Json<PullReq>) -> Json<PullResp> {
    match pull(&app, &req).await {
        Ok(pulled) => Json(PullResp { ok: true, pulled, ..Default::default() }),
        Err(e) => Json(PullResp { message: format!("{e:#}"), ..Default::default() }),
    }
}

async fn pull(app: &AppState, req: &PullReq) -> Result<Vec<String>> {
    ensure!(is_safe_token(&req.session_id), "Invalid session");
    ensure!(!req.paths.is_empty(), "没有选择文件");
    if let Some(why) = crate::ssh::too_broad_project_dir(&app.project_dir) {
        bail!("{why}");
    }
    ensure!(req.paths.len() <= 500, "一次最多取回 500 个文件");
    ensure!(req.paths.iter().all(|p| safe_rel(p)), "文件路径不合法");
    let config = app.global();
    let state = crate::storage::ProjectState::load(&app.project_id)?;
    let session = state.list_sessions().find(|s| s.id == req.session_id).context("Session not found")?;
    let server = config.servers.get(&session.server).context("Server not found")?.clone();
    let work = remote_work_path(&server, &req.session_id);

    let list = std::env::temp_dir().join(format!("yunpao-pull-{}.txt", crate::utils::random_uuid()));
    std::fs::write(&list, req.paths.join("\n") + "\n")?;
    // BatchMode: no terminal to answer a password prompt; fail instead of hanging.
    let mut ssh = format!("ssh -o BatchMode=yes -o ConnectTimeout=10 {} -p {}", crate::ssh::mux_opts(&server.host).join(" "), server.port);
    if let Some(id) = &server.identity {
        ssh.push_str(&format!(" -i {}", shellexpand::tilde(id)));
    }
    // -u: never replace a local file that is newer than the server's copy.
    let out = tokio::process::Command::new("rsync")
        .arg("-azu")
        .arg(format!("--files-from={}", list.display()))
        .args(["-e", &ssh])
        .arg(format!("{}@{}:{}/", server.user, server.host, work))
        .arg(format!("{}/", app.project_dir.display()))
        .output()
        .await;
    let _ = std::fs::remove_file(&list);
    let out = out.context("Could not run rsync")?;
    if !out.status.success() {
        bail!("rsync 失败：{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let _ = append_record("pulls", &app.project_id, PullRecord {
        time: chrono::Local::now().timestamp(),
        session_id: req.session_id.clone(),
        files: req.paths.clone(),
    }, 500);
    Ok(req.paths.clone())
}

#[derive(Deserialize)]
pub struct PreviewQuery {
    path: String,
}

/// Preview a file already in the local project folder: images as-is, text as its first lines.
pub async fn api_preview(
    State(app): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<PreviewQuery>,
) -> axum::response::Response {
    use axum::{http::{StatusCode, header}, response::IntoResponse};
    if !safe_rel(&q.path) {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    let full = app.project_dir.join(&q.path);
    let (Ok(full), Ok(root)) = (full.canonicalize(), app.project_dir.canonicalize()) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if !full.starts_with(&root) {
        return (StatusCode::FORBIDDEN, "outside project").into_response();
    }
    let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    // No SVG: it can carry scripts and this page has access to the local API.
    let image = match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    };
    // Read only what the preview shows, off the async workers (results can be multi-GB).
    let read_head = |path: std::path::PathBuf, limit: u64| async move {
        tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            let mut buf = Vec::new();
            std::fs::File::open(path)?.take(limit).read_to_end(&mut buf)?;
            Ok(buf)
        })
        .await
        .ok()
        .and_then(Result::ok)
    };
    if let Some(ct) = image {
        const MAX_IMAGE: u64 = 20 * 1024 * 1024;
        if std::fs::metadata(&full).map(|m| m.len() > MAX_IMAGE).unwrap_or(true) {
            return (StatusCode::PAYLOAD_TOO_LARGE, "image too large to preview").into_response();
        }
        return match read_head(full, MAX_IMAGE).await {
            Some(bytes) => ([(header::CONTENT_TYPE, ct), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], bytes).into_response(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }
    if matches!(ext.as_str(), "txt" | "csv" | "tsv" | "log" | "json" | "md" | "yaml" | "yml" | "out") {
        let bytes = read_head(full, 20_000).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        let head: String = text.lines().take(60).collect::<Vec<_>>().join("\n");
        return Json(serde_json::json!({ "text": head })).into_response();
    }
    (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no preview").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_run_logs() {
        let out = "F /home/u/.cache/yunpao/sessions/9e65-ab/logs/runs/voice-20261109-145747-d0e7.log\n\
                   [yunpao] Started: 2026-11-09 14:57:49\n\
                   [yunpao] Command: export PATH=/home/u/envs/rvc/bin:$PATH; export CUDA_DEVICE_ORDER=PCI_BUS_ID; export CUDA_VISIBLE_DEVICES=4,5; python convert.py --pitch 12\n\
                   F /home/u/.cache/yunpao/sessions/x/logs/runs/not-a-run.log\n\
                   F /home/u/.cache/yunpao/sessions/s2/logs/runs/my-train-20261101-090000-aaaa.log\n\
                   [yunpao] Started: 2026-11-01 09:00:00\n\
                   [yunpao] Command: bash run.sh\n";
        let runs = parse_server_runs(out);
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].session_id.as_str(), runs[0].name.as_str(), runs[0].run_id.as_str()), ("9e65-ab", "voice", "20261109-145747-d0e7"));
        assert_eq!(runs[0].command, "python convert.py --pitch 12");
        assert_eq!(runs[0].python_env.as_deref(), Some("/home/u/envs/rvc"));
        assert_eq!(runs[0].gpus, vec![4, 5]);
        assert_eq!((runs[1].name.as_str(), runs[1].command.as_str()), ("my-train", "bash run.sh"));
    }

    #[test]
    fn log_times_use_the_servers_zone() {
        // New format: zone in the line.
        assert_eq!(parse_log_time("2026-11-10 10:00:00 +0000", None), Some(1794304800));
        assert_eq!(parse_log_time("2026-11-10 18:00:00 +0800", None), Some(1794304800));
        // Old format: the server's zone from the TZ line wins over this computer's.
        let runs = parse_server_runs("TZ +0000\nF /h/.cache/yunpao/sessions/s/logs/runs/a-20261110-100000-abcd.log\n[yunpao] Started: 2026-11-10 10:00:00\n");
        assert_eq!(runs[0].time, 1794304800);
    }

    #[test]
    fn traceback_maps_to_local_source() {
        let dir = std::env::temp_dir().join(format!("yunpao-tb-{}", crate::utils::random_uuid()));
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("main.py"), "import pkg.helper\npkg.helper.f()\n").unwrap();
        std::fs::write(dir.join("pkg/helper.py"), "def f():\n    return [][1]\n").unwrap();
        // Home is a symlink on this server, so Python prints /data/home/... not /home/...
        let log = r#"Traceback (most recent call last):
  File "<frozen runpy>", line 198, in _run_module_as_main
  File "/data/home/u/.cache/yunpao/sessions/abc-123/working/main.py", line 2, in <module>
    pkg.helper.f()
  File "/data/home/u/.cache/yunpao/sessions/abc-123/working/pkg/helper.py", line 2, in f
    return [][1]
  File "/usr/lib/python3.12/site-packages/x.py", line 9, in g
  File "/data/home/u/.cache/yunpao/sessions/abc-123/working/only_remote.py", line 1, in h
IndexError: list index out of range"#;
        let frames = traceback_frames(log, "abc-123");
        assert_eq!(
            frames,
            vec![("main.py".into(), 2), ("pkg/helper.py".into(), 2), ("only_remote.py".into(), 1)]
        );
        let src = source_snippets(&dir, &frames);
        assert!(src.contains("pkg/helper.py:\n"), "{src}");
        assert!(src.contains(">>    2 |     return [][1]"), "{src}");
        assert!(src.contains("main.py:\n"), "{src}");
        assert!(!src.contains("only_remote"), "{src}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
