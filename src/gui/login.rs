//! Password login from the page, for people who have never opened a terminal.
//!
//! ssh is run with `SSH_ASKPASS` pointing back at this binary (see [`askpass`]), which hands
//! it the password from its environment. That opens a ControlMaster connection that lives
//! for 4 hours; every later command rides on it, so the password is used once and never
//! written to disk.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::{io::Write, sync::Arc};

use super::AppState;
use crate::config::{Global, HostArgs, Server};

const ASKPASS_ENV: &str = "YUNPAO_ASKPASS";
/// Path of a 0600 file holding the password, deleted as soon as ssh has authenticated.
/// (An environment variable would live on in the backgrounded ControlMaster for hours.)
const PW_ENV: &str = "YUNPAO_ASKPASS_PWFILE";
const OUT_ENV: &str = "YUNPAO_ASKPASS_OUT";
/// Written instead of a password prompt, so the handler knows a password was asked for.
const PW_MARK: &str = "<password asked>";

/// Entry point when ssh runs us as its askpass program (`yunpao` with YUNPAO_ASKPASS set).
/// Answers password prompts; any other prompt (host key, 2FA code) is recorded for the
/// login handler and refused, so ssh fails instead of guessing.
pub fn askpass_mode() -> bool {
    if std::env::var_os(ASKPASS_ENV).is_none() {
        return false;
    }
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let low = prompt.to_lowercase();
    let is_password = low.contains("password") || low.contains("密码");
    if let Some(out) = std::env::var_os(OUT_ENV)
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(out)
    {
        let _ = writeln!(f, "{}\n---", if is_password { PW_MARK } else { prompt.as_str() });
    }
    if is_password {
        let pw = std::env::var_os(PW_ENV).and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        println!("{pw}");
    } else {
        println!("{}", if low.contains("yes/no") { "no" } else { "" });
    }
    true
}

#[derive(Deserialize)]
pub struct LoginReq {
    /// An existing server; otherwise `host`/`user`/`port` describe a new one.
    #[serde(default)]
    server_id: Option<String>,
    #[serde(default)]
    host: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    port: Option<u16>,
    password: String,
    /// The person checked the fingerprint we showed them.
    #[serde(default)]
    accept_host_key: bool,
}

#[derive(Serialize, Default)]
pub struct LoginResp {
    ok: bool,
    /// "ok" | "hostkey" (confirm fingerprint) | "hostkey_changed" | "bad_password" | "2fa" |
    /// "no_password" | "unreachable" | "invalid" | "failed"
    code: &'static str,
    message: String,
    /// For "hostkey": the fingerprint line to show.
    fingerprint: String,
    server_id: String,
}

fn resp(code: &'static str, message: impl Into<String>) -> Json<LoginResp> {
    Json(LoginResp { ok: code == "ok", code, message: message.into(), ..Default::default() })
}

fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() < 256 && !h.starts_with('-') && h.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
}

fn valid_user(u: &str) -> bool {
    !u.is_empty() && u.len() < 64 && !u.starts_with('-') && u.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

pub async fn api_login(State(app): State<Arc<AppState>>, Json(req): Json<LoginReq>) -> Json<LoginResp> {
    if req.password.len() > 1024 || req.password.contains('\n') {
        return resp("invalid", "密码格式不对（太长或包含换行）");
    }
    let existing = req.server_id.as_ref().and_then(|id| app.global().servers.get(id).cloned());
    let server = match (&req.server_id, existing) {
        (Some(_), Some(s)) => s,
        (Some(_), None) => return resp("invalid", "找不到这台服务器"),
        (None, _) => {
            let (host, user) = (req.host.trim(), req.user.trim());
            if !valid_host(host) {
                return resp("invalid", "服务器地址只能是域名或 IP，比如 202.120.38.31");
            }
            if !valid_user(user) {
                return resp("invalid", "用户名只能包含英文字母、数字、. _ -");
            }
            let port = req.port.unwrap_or(22).to_string();
            match Server::new(HostArgs::FromData { user, host, port: &port }, None) {
                Ok(s) => s,
                Err(e) => return resp("invalid", format!("{e:#}")),
            }
        }
    };

    // Already logged in (a master is alive): nothing to do, and a second master can't bind.
    let mut check: Vec<String> = crate::ssh::mux_opts(&server.host);
    check.extend(["-p".into(), server.port.to_string(), "-O".into(), "check".into(), format!("{}@{}", server.user, server.host)]);
    let alive = tokio::process::Command::new("ssh").args(&check).stdin(std::process::Stdio::null()).output().await
        .is_ok_and(|o| o.status.success());
    if alive && req.server_id.is_some() {
        return Json(LoginResp { ok: true, code: "ok", server_id: server.id.clone(), message: "已经登录过了".into(), ..Default::default() });
    }

    // Prompts the askpass helper refused end up in this file (in our 0700 socket dir).
    let out = crate::ssh::control_dir().and_then(|d| PromptFile::create(&d));
    let exe = std::env::current_exe().ok();
    let pw_file = crate::ssh::control_dir().and_then(|d| PromptFile::create(&d));
    let (Some(out), Some(exe), Some(pw_file)) = (out, exe, pw_file) else { return resp("failed", "无法启动登录程序") };
    if std::fs::write(&pw_file.0, &req.password).is_err() {
        return resp("failed", "无法启动登录程序");
    }

    let mut args: Vec<String> = vec![
        "-o".into(), "ConnectTimeout=10".into(),
        "-o".into(), "NumberOfPasswordPrompts=1".into(),
        "-o".into(), "ControlMaster=yes".into(),
        "-o".into(), "ControlPersist=4h".into(),
        "-p".into(), server.port.to_string(),
    ];
    // Same socket the rest of yunpao will look for (the user's own ControlPath if any).
    args.extend(
        crate::ssh::mux_opts(&server.host)
            .chunks(2)
            .filter(|kv| kv[1].starts_with("ControlPath="))
            .flatten()
            .cloned(),
    );
    if req.accept_host_key {
        args.extend(["-o".into(), "StrictHostKeyChecking=accept-new".into()]);
    }
    if let Some(id) = &server.identity {
        args.extend(["-i".into(), shellexpand::tilde(id).into_owned()]);
    }
    args.extend(["-N".into(), "-f".into(), format!("{}@{}", server.user, server.host)]);

    let output = tokio::process::Command::new("ssh")
        .args(&args)
        .env("SSH_ASKPASS", &exe)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env(ASKPASS_ENV, "1")
        .env(PW_ENV, &pw_file.0)
        .env(OUT_ENV, &out.0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(std::time::Duration::from_secs(40), output).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return resp("failed", e.to_string()),
        Err(_) => return resp("unreachable", "登录超时。确认连着校园网或 VPN，再试一次。"),
    };
    drop(pw_file); // ssh -f has authenticated (or given up): the password file goes now
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let log = std::fs::read_to_string(&out.0).unwrap_or_default();
    let asked_password = log.contains(PW_MARK);
    let prompts: String = log.split("\n---").filter(|p| !p.trim().is_empty() && !p.contains(PW_MARK)).collect::<Vec<_>>().join("\n---");

    if output.status.success() {
        let mut server_id = server.id.clone();
        if req.server_id.is_none() {
            match save_server(server).await {
                Ok(id) => server_id = id,
                Err(e) => return resp("failed", format!("登录成功，但保存服务器失败：{e}")),
            }
        }
        return Json(LoginResp { ok: true, code: "ok", server_id, ..Default::default() });
    }

    if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return resp("hostkey_changed",
            "这台服务器的身份和上次记录的不一样。可能是服务器重装了，也可能有人在冒充它。先问一下管理员，确认没问题再处理（终端里会显示要删哪一行）。");
    }
    if let Some(p) = prompts.split("\n---").find(|p| p.contains("yes/no")) {
        let fingerprint = p.lines().find(|l| l.contains("fingerprint")).unwrap_or("").trim().to_string();
        return Json(LoginResp {
            code: "hostkey",
            message: "第一次连这台服务器，需要确认它的身份。".into(),
            fingerprint,
            ..Default::default()
        });
    }
    if !prompts.trim().is_empty() {
        return resp("2fa", format!("这台服务器除了密码还要别的验证（{}），网页登录暂不支持，请在终端里登录一次。",
            prompts.split("\n---").next().unwrap_or("").trim()));
    }
    if stderr.contains("Permission denied") {
        return if asked_password {
            resp("bad_password", "用户名或密码不对。")
        } else {
            resp("no_password", "这台服务器不允许用密码登录，只能用密钥。请找管理员要登录方式，或在终端里配置密钥。")
        };
    }
    if ["timed out", "No route", "Could not resolve", "Connection refused", "Network is unreachable"].iter().any(|k| stderr.contains(k)) {
        return resp("unreachable", format!("连不上服务器：{}。校内服务器要连校园网或 VPN；也检查一下地址和端口。", last_line(&stderr)));
    }
    resp("failed", last_line(&stderr))
}

/// A file removed when dropped.
struct PromptFile(std::path::PathBuf);

impl PromptFile {
    fn create(dir: &std::path::Path) -> Option<Self> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
        let path = dir.join(format!("askpass-{}-{nanos}", std::process::id()));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(&path).ok()?;
        Some(Self(path))
    }
}

impl Drop for PromptFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn last_line(s: &str) -> String {
    s.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("**")).last().unwrap_or("登录失败").trim().to_string()
}

/// Adds a newly logged-in server (unless the same user@host:port is already there).
async fn save_server(server: Server) -> Result<String, String> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let mut config = Global::load()?;
        if let Some((id, _)) = config
            .servers
            .iter()
            .find(|(_, s)| s.host == server.host && s.user == server.user && s.port == server.port)
        {
            return Ok(id.clone());
        }
        let id = server.id.clone();
        config.add_server(server)?;
        if config.default_server.is_none() {
            config.set_default_server(&id)?;
        }
        Ok(id)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}
