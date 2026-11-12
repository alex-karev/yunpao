//! A shell on the server, in the page: `ssh -tt` on a local pseudo-terminal, bridged to
//! xterm.js over a WebSocket. Lands in the project's code folder on the server.
//!
//! Browsers don't apply same-origin rules to WebSockets, so any website could open one to
//! localhost; the Origin check below is what keeps them out of the user's shell.

use axum::{
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::Deserialize;
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
};

use super::{AppState, ssh_base_args};
use crate::config::Server;

#[derive(Deserialize)]
pub struct TermQuery {
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

/// Messages from the page. Output goes the other way as binary frames.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum ClientMsg {
    /// Keystrokes / paste.
    #[serde(rename = "i")]
    Input { d: String },
    #[serde(rename = "r")]
    Resize { c: u16, r: u16 },
}

pub async fn api_terminal(
    State(app): State<Arc<AppState>>,
    Path(server_id): Path<String>,
    Query(q): Query<TermQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("");
    if ![format!("http://localhost:{}", app.port), format!("http://127.0.0.1:{}", app.port)].iter().any(|o| o == origin) {
        return (StatusCode::FORBIDDEN, "forbidden origin").into_response();
    }
    let Some(server) = app.global().servers.get(&server_id).cloned() else {
        return (StatusCode::NOT_FOUND, "server not found").into_response();
    };
    let workdir = project_workdir(&app, &server_id, &server);
    let size = PtySize { cols: q.cols.unwrap_or(100).clamp(20, 500), rows: q.rows.unwrap_or(30).clamp(5, 200), pixel_width: 0, pixel_height: 0 };
    ws.on_upgrade(move |socket| bridge(socket, server, workdir, size))
}

/// The project's code folder on this server: its current session there, else the newest.
fn project_workdir(app: &AppState, server_id: &str, server: &Server) -> Option<String> {
    let state = crate::storage::ProjectState::load(&app.project_id).ok()?;
    let current = state.current_session().filter(|s| s.server == server_id).map(|s| s.id.clone());
    let id = current.or_else(|| state.list_sessions().filter(|s| s.server == server_id).max_by_key(|s| s.created_at).map(|s| s.id.clone()))?;
    Some(super::jobs::remote_work_path(server, &id))
}

async fn bridge(socket: WebSocket, server: Server, workdir: Option<String>, size: PtySize) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let fail = |m: String| Message::Text(format!("\r\n\x1b[31m{m}\x1b[0m\r\n"));

    let pty = match native_pty_system().openpty(size) {
        Ok(p) => p,
        Err(e) => return drop(ws_tx.send(fail(format!("无法创建终端：{e}"))).await),
    };
    // Same connection options as the rest of the page, minus BatchMode: here a password
    // prompt is fine, the person can type it.
    let mut args = ssh_base_args(&server);
    let dest = args.pop().unwrap_or_default();
    if let Some(i) = args.iter().position(|a| a == "BatchMode=yes") {
        args.drain(i - 1..=i);
    }
    let mut cmd = CommandBuilder::new("ssh");
    cmd.args(&args);
    cmd.arg("-tt");
    cmd.arg(dest);
    let remote = match &workdir {
        Some(dir) => format!(
            "if cd {} 2>/dev/null; then echo \"📁 已进入项目代码目录：$PWD\"; else echo '项目代码还没传到这台服务器（提交一次任务就会上传），先待在主目录。'; fi; exec \"$SHELL\" -l",
            shlex::try_quote(dir).unwrap_or_default()
        ),
        None => "echo '还没在这台服务器上提交过这个项目，先待在主目录。'; exec \"$SHELL\" -l".into(),
    };
    cmd.arg(remote);
    cmd.env("TERM", "xterm-256color");

    let mut child = match pty.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => return drop(ws_tx.send(fail(format!("无法启动 ssh：{e}"))).await),
    };
    drop(pty.slave);
    let master = Arc::new(Mutex::new(pty.master));
    let (Ok(mut reader), Ok(mut writer)) = ({ let m = master.lock().unwrap(); (m.try_clone_reader(), m.take_writer()) }) else {
        let _ = child.kill();
        return;
    };

    // pty → page
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || out_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    // page → pty (blocking writes stay off the async runtime)
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for bytes in in_rx {
            if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
    });

    let send_out = async {
        while let Some(bytes) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                return;
            }
        }
        let _ = ws_tx.send(Message::Text("\r\n\x1b[2m[连接已结束]\x1b[0m\r\n".into())).await;
        let _ = ws_tx.close().await;
    };
    let read_in = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Text(t) => match serde_json::from_str::<ClientMsg>(&t) {
                    Ok(ClientMsg::Input { d }) => {
                        if in_tx.send(d.into_bytes()).is_err() {
                            return;
                        }
                    }
                    Ok(ClientMsg::Resize { c, r }) => {
                        let _ = master.lock().unwrap().resize(PtySize { cols: c.clamp(20, 500), rows: r.clamp(5, 200), pixel_width: 0, pixel_height: 0 });
                    }
                    Err(_) => {}
                },
                Message::Close(_) => return,
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = send_out => {}
        _ = read_in => {}
    }
    // Page closed or shell exited: don't leave ssh running.
    let _ = child.kill();
    let _ = child.wait();
}

const XTERM_JS: &str = include_str!("vendor/xterm.js");
const XTERM_CSS: &str = include_str!("vendor/xterm.css");
const FIT_JS: &str = include_str!("vendor/addon-fit.js");

/// xterm.js ships inside the binary: the page works offline / on campus networks.
pub async fn vendor(Path(file): Path<String>) -> Response {
    let (body, ty) = match file.as_str() {
        "xterm.js" => (XTERM_JS, "text/javascript"),
        "xterm.css" => (XTERM_CSS, "text/css"),
        "addon-fit.js" => (FIT_JS, "text/javascript"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, ty), (header::CACHE_CONTROL, "max-age=86400")], body).into_response()
}
