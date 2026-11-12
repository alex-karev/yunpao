//! "Wait for free GPUs": a job is held here and submitted as soon as enough GPUs are idle.
//! Lives in this `yunpao ui` process: it survives closing the page, not quitting yunpao.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use super::{AppState, GpuInfo, gpu_stats, jobs::{JobReq, invalid_job, record_not_run, submit}, server_label};

const POLL: Duration = Duration::from_secs(30);

#[derive(Serialize, Clone)]
pub struct QueueItem {
    id: u64,
    server_id: String,
    server_label: String,
    name: String,
    command: String,
    want: u32,
    created: i64,
    /// "waiting" | "starting" | "started" | "failed" | "cancelled"
    state: &'static str,
    message: String,
    gpus: Vec<u32>,
    session_id: String,
    python_env: Option<String>,
}

static QUEUE: Mutex<Vec<QueueItem>> = Mutex::new(Vec::new());

fn update(id: u64, f: impl FnOnce(&mut QueueItem)) {
    if let Some(it) = QUEUE.lock().unwrap().iter_mut().find(|i| i.id == id) {
        f(it);
    }
}

fn state_of(id: u64) -> Option<&'static str> {
    QUEUE.lock().unwrap().iter().find(|i| i.id == id).map(|i| i.state)
}

/// GPUs just handed to a job: for the first minutes (upload, imports, model loading) they
/// still look idle, so nothing else may pick them meanwhile.
const RESERVE_FOR: Duration = Duration::from_secs(300);
static RESERVED: Mutex<Vec<(String, u32, std::time::Instant)>> = Mutex::new(Vec::new());

pub(super) fn reserve(server_id: &str, gpus: &[u32]) {
    let mut r = RESERVED.lock().unwrap();
    r.retain(|(_, _, t)| t.elapsed() < RESERVE_FOR);
    r.extend(gpus.iter().map(|&g| (server_id.to_string(), g, std::time::Instant::now())));
}

fn reserved(server_id: &str) -> Vec<u32> {
    let mut r = RESERVED.lock().unwrap();
    r.retain(|(_, _, t)| t.elapsed() < RESERVE_FOR);
    r.iter().filter(|(s, ..)| s == server_id).map(|(_, g, _)| *g).collect()
}

/// Waiting → starting, only if it's still waiting (not cancelled meanwhile).
fn claim(id: u64, gpus: &[u32]) -> bool {
    let mut q = QUEUE.lock().unwrap();
    match q.iter_mut().find(|i| i.id == id && i.state == "waiting") {
        Some(it) => {
            it.state = "starting";
            it.gpus = gpus.to_vec();
            it.message = String::new();
            true
        }
        None => false,
    }
}

/// Same rule as the page's `isFree`, minus reserved cards; least-used first.
fn pick_free(gpus: &[GpuInfo], want: usize, taken: &[u32]) -> Option<Vec<u32>> {
    let mut free: Vec<&GpuInfo> = gpus.iter().filter(|g| g.util_gpu < 10 && g.mem_used < 1024 && !taken.contains(&g.index)).collect();
    free.sort_by_key(|g| g.mem_used);
    (free.len() >= want).then(|| free[..want].iter().map(|g| g.index).collect())
}

#[derive(Serialize)]
pub struct EnqueueResp {
    ok: bool,
    message: String,
}

pub async fn api_enqueue(State(app): State<Arc<AppState>>, Json(req): Json<JobReq>) -> Json<EnqueueResp> {
    let fail = |m: &str| Json(EnqueueResp { ok: false, message: m.into() });
    if let Some(m) = invalid_job(&req) {
        return fail(m);
    }
    let config = app.global();
    let Some(server) = config.servers.get(&req.server_id).cloned() else { return fail("找不到这台服务器") };
    if req.wait_gpus == 0 || req.wait_gpus > 16 {
        return fail("要等的显卡数量不对");
    }
    let id = {
        let mut q = QUEUE.lock().unwrap();
        if q.iter().any(|i| i.name == req.name.trim() && matches!(i.state, "waiting" | "starting")) {
            return fail("已经有一个同名任务在排队了");
        }
        let id = q.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        q.push(QueueItem {
            id,
            server_id: req.server_id.clone(),
            server_label: server_label(&server),
            name: req.name.trim().to_string(),
            command: req.command.trim().to_string(),
            want: req.wait_gpus,
            created: chrono::Local::now().timestamp(),
            state: "waiting",
            message: String::new(),
            gpus: Vec::new(),
            session_id: String::new(),
            python_env: req.python_env.clone(),
        });
        id
    };

    tokio::spawn(async move {
        loop {
            if state_of(id) != Some("waiting") {
                return;
            }
            match gpu_stats(&server).await {
                Ok(gpus) => {
                    if let Some(picked) = pick_free(&gpus, req.wait_gpus as usize, &reserved(&req.server_id)) {
                        // Cancelled while we were asking the server? Then stop here.
                        if !claim(id, &picked) {
                            return;
                        }
                        reserve(&req.server_id, &picked);
                        let mut job = req.clone();
                        job.gpus = picked;
                        let (name, command) = (job.name.trim().to_string(), job.command.trim().to_string());
                        let app2 = app.clone();
                        let r = tokio::task::spawn_blocking(move || submit(&app2, &job, &name, &command, &mut |_| {})).await;
                        match r {
                            Ok(Ok(resp)) if resp.ok => update(id, |i| { i.state = "started"; i.session_id = resp.session_id; }),
                            Ok(Ok(resp)) => update(id, |i| { i.state = "failed"; i.message = resp.message; }),
                            Ok(Err(e)) => update(id, |i| { i.state = "failed"; i.message = format!("{e:#}"); }),
                            Err(e) => update(id, |i| { i.state = "failed"; i.message = e.to_string(); }),
                        }
                        return;
                    }
                    let taken = reserved(&req.server_id);
                    let free = gpus.iter().filter(|g| g.util_gpu < 10 && g.mem_used < 1024 && !taken.contains(&g.index)).count();
                    update(id, |i| i.message = format!("现在空闲 {free} 张，还差 {} 张", i.want as usize - free.min(i.want as usize)));
                }
                Err(e) => update(id, |i| i.message = format!("暂时连不上服务器（{e}），稍后重试")),
            }
            tokio::time::sleep(POLL).await;
        }
    });
    Json(EnqueueResp { ok: true, message: String::new() })
}

pub async fn api_queue() -> Json<Vec<QueueItem>> {
    Json(QUEUE.lock().unwrap().clone())
}

#[derive(Deserialize)]
pub struct CancelReq {
    id: u64,
}

/// Cancels a waiting item, or dismisses a finished one from the list.
pub async fn api_cancel(State(app): State<Arc<AppState>>, Json(req): Json<CancelReq>) -> Json<EnqueueResp> {
    let mut q = QUEUE.lock().unwrap();
    match q.iter().position(|i| i.id == req.id) {
        Some(p) if q[p].state == "starting" => Json(EnqueueResp { ok: false, message: "已经在提交了，来不及取消".into() }),
        Some(p) => {
            let it = q.remove(p);
            // A cancelled wait never reaches submit(), so record it here.
            if it.state == "waiting" {
                let job = JobReq {
                    server_id: it.server_id, command: it.command, name: it.name, gpus: Vec::new(),
                    python_env: it.python_env, wait_gpus: it.want,
                };
                record_not_run(&app, &job, &it.server_label, "cancelled", &format!("排队等 {} 张空卡时取消了", it.want));
            }
            Json(EnqueueResp { ok: true, message: String::new() })
        }
        None => Json(EnqueueResp { ok: true, message: String::new() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(index: u32, util_gpu: u32, mem_used: u32) -> GpuInfo {
        GpuInfo { index, util_gpu, mem_used, ..Default::default() }
    }

    #[test]
    fn picks_least_used_free_gpus() {
        let g = [gpu(0, 90, 20000), gpu(1, 0, 500), gpu(2, 0, 10), gpu(3, 5, 2000)];
        assert_eq!(pick_free(&g, 2, &[]), Some(vec![2, 1]));
        assert_eq!(pick_free(&g, 3, &[]), None);
        assert_eq!(pick_free(&g, 1, &[2]), Some(vec![1]));
    }
}
