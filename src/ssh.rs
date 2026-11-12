use crate::config::{Global, Project, Server};
use anyhow::{Context, Result};
use dotenvy::dotenv_iter;
use log;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, PartialEq)]
enum SSHMode {
    Interactive,
    StdOut,
    Raw,
}

#[derive(Debug, PartialEq)]
pub enum RemoteCommand<'a> {
    Action(&'a String),
    Exec(&'a String),
    /// Arbitrary command tracked under an explicit task name.
    Named { name: &'a String, command: &'a String },
}

#[derive(Debug)]
pub struct RemoteContext<'a> {
    workdir: String,
    logdir: String,
    home: String,
    server: &'a Server,
    artifacts: Vec<String>,
    env: HashMap<String, String>,
    /// Extra ssh options (e.g. BatchMode for callers with no terminal to prompt on).
    ssh_opts: Vec<String>,
}

impl<'a> RemoteContext<'a> {
    /// Build new remote context
    pub fn new(config: &'a Global, project: &Project) -> Result<Self> {
        let session = project
            .state
            .current_session()
            .context("No active session. Start new session using \"yunpao session new\"")?;
        Self::for_session(config, project, session)
    }

    /// Build a remote context for a specific session (not necessarily the active one).
    pub fn for_session(config: &'a Global, project: &Project, session: &crate::storage::Session) -> Result<Self> {
        // Get server
        let server = config.find_server(&session.server)?;
        // Resolve paths
        let home = remote_home(server);
        let basedir = format!("{}/.cache/yunpao/sessions/{}", &home, &session.id);
        let workdir = format!("{}/working", &basedir);
        let logdir = format!("{}/logs", &basedir);
        // Set env
        let mut env = HashMap::from([
            ("YUNPAO_WORKDIR".to_string(), workdir.clone()),
            ("YUNPAO_LOGDIR".to_string(), logdir.clone()),
        ]);
        for (k, v) in config.env.iter().chain(server.env.iter()) {
            env.insert(k.clone(), v.clone());
        }
        if !project.disable_dotenv
            && let Some(dotenv) = dotenv_iter().ok()
        {
            for item in dotenv {
                if let Some((k, v)) = item.ok() {
                    env.insert(k, v);
                }
            }
        }
        // Expand artifact paths
        let artifacts: Vec<String> = project
            .artifacts
            .iter()
            .map(|path| shellexpand::tilde_with_context(&path, || Some(&home)).to_string())
            .collect();
        // Construct context
        Ok(Self {
            home,
            workdir,
            logdir,
            server,
            artifacts,
            env,
            ssh_opts: Vec::new(),
        })
    }

    /// Never prompt for a password or host key: fail fast instead. For the web UI, where
    /// a prompt would land on the terminal running `yunpao ui` and hang the request.
    pub fn batch_mode(mut self) -> Self {
        self.ssh_opts = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"].map(String::from).to_vec();
        self.ssh_opts.extend(mux_opts(&self.server.host));
        self
    }

    // Build base for ssh command
    fn build_ssh_command<T: std::fmt::Display>(&self, command: T, mode: SSHMode) -> Command {
        // Build run script
        let mut remote_command = format!(
            "set -e && mkdir -p {workdir} && mkdir -p {logdir} && cd {workdir} &&",
            workdir = self.workdir,
            logdir = self.logdir
        );
        for (k, v) in self.env.iter() {
            remote_command = format!("{remote_command} export {k}=\"{v}\" &&");
        }
        remote_command = format!("{remote_command} {command}");
        // Wrap
        remote_command = if mode != SSHMode::StdOut {
            remote_command
        } else {
            format!("echo '{remote_command}' | bash")
        };
        // Build command
        let mut command = Command::new("ssh");
        command.args(&self.ssh_opts);
        if mode == SSHMode::Interactive {
            command.arg("-t");
        }
        if let Some(identity) = self.server.identity.as_ref() {
            let identity_path = shellexpand::tilde(identity);
            command.arg("-i");
            command.arg(identity_path.to_string());
        }
        command.arg(format!("-p {}", &self.server.port));
        command.arg(format!(
            "{user}@{host}",
            user = &self.server.user,
            host = &self.server.host,
        ));
        command.arg(remote_command);
        command
    }

    // Rsync command base
    fn build_rsync_command(&self) -> Result<Command> {
        self.rsync_base("-azP", "%n%L")
    }

    fn rsync_base(&self, flags: &str, out_format: &str) -> Result<Command> {
        // Define dirs
        let current_dir = std::env::current_dir()?;
        if let Some(why) = too_broad_project_dir(&current_dir) {
            anyhow::bail!("{why}");
        }
        let mut gitdir = current_dir.clone();
        gitdir.push(".git");
        // Define ssh base command
        let mut ssh_command = format!("ssh {} -p {}", self.ssh_opts.join(" "), &self.server.port);
        if let Some(identity) = self.server.identity.as_ref() {
            ssh_command = format!("{} -i {}", ssh_command, identity);
        }
        // Build command
        let mut command = Command::new("rsync");
        // No --mkpath: macOS ships openrsync, which rejects it. push() creates the
        // remote dir through --rsync-path instead, which works with any client.
        command.arg(flags);
        command.arg(format!("--out-format={out_format}"));
        command.arg("-e");
        command.arg(ssh_command);
        if gitdir.exists() {
            command.arg("-f");
            command.arg("- /.git/");
        }
        Ok(command)
    }

    /// Source, destination and excludes of an upload. `dest` overrides the session's
    /// working directory (used by previews, which must not create anything remotely).
    fn push_args(&self, command: &mut Command, dest: Option<&str>) -> Result<()> {
        let current_dir = std::env::current_dir()?;
        let target = dest.unwrap_or(&self.workdir);
        if dest.is_none() {
            command.arg(format!("--rsync-path=mkdir -p {} && rsync", &self.workdir));
        }
        let gitignore = current_dir.join(".gitignore");
        if gitignore.exists() {
            command.arg(format!("--exclude-from={}", gitignore.to_string_lossy()));
        }
        command.arg(format!("{}/", current_dir.to_string_lossy()));
        command.arg(format!("{}@{}:{}", &self.server.user, &self.server.host, target));
        Ok(())
    }

    /// Files an upload would send right now (dry run), relative to the project folder.
    /// `fresh`: plan as if nothing had been uploaded yet (no session on that server).
    pub fn push_plan(&self, fresh: bool) -> Result<Vec<String>> {
        let mut command = self.rsync_base("-azn", "%n")?;
        let scratch = format!("/tmp/yunpao-preview-{}", crate::utils::random_uuid());
        self.push_args(&mut command, fresh.then_some(scratch.as_str()))?;
        let out = command.stdin(Stdio::null()).output()?;
        check_rsync_status(out.status, &out.stderr)?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && *l != "./" && !l.ends_with('/'))
            .map(String::from)
            .collect())
    }

    /// Upload, calling `on_line` with each line rsync prints: a file name when it starts
    /// sending that file, then `--progress` lines ("  9666560  32%  17.08MB/s ...") for it,
    /// plus any messages. Callers match names against push_plan().
    pub fn push_with_progress(&self, mut on_line: impl FnMut(&str)) -> Result<()> {
        use std::io::Read;
        let mut command = self.rsync_base("-az", "%n")?;
        command.arg("--progress");
        self.push_args(&mut command, None)?;
        // rsync (macOS's openrsync at least) buffers its output when piped, so names would
        // only arrive at the very end. A pseudo-terminal via `script` makes them stream.
        let mut command = with_pty(command);
        let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let mut out = child.stdout.take().context("no stdout")?;
        let mut lines_seen: Vec<String> = Vec::new();
        let mut handle = |raw: &[u8]| {
            let text = String::from_utf8_lossy(raw);
            // A pty echoes ^D (plus backspaces) for the closed stdin.
            let line = text.trim_matches(|c: char| c.is_control() || c.is_whitespace()).trim_start_matches("^D");
            let line = line.trim_start_matches(|c: char| c.is_control());
            if !line.is_empty() && line != "./" && !line.ends_with('/') {
                on_line(line);
                lines_seen.push(line.to_string());
                if lines_seen.len() > 50 {
                    lines_seen.remove(0);
                }
            }
        };
        // Progress updates end in \r, not \n.
        let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
        loop {
            let n = out.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            for &b in &chunk[..n] {
                if b == b'\r' || b == b'\n' {
                    handle(&buf);
                    buf.clear();
                } else {
                    buf.push(b);
                }
            }
        }
        handle(&buf);
        let status = child.wait()?;
        check_rsync_status(status, lines_seen.join("\n").as_bytes())
    }

    // Pull single artifact
    fn pull_artifact(&self, artifact: &String) -> Result<()> {
        // Define path
        let current_dir = std::env::current_dir()?;
        let path = if artifact.contains("$") {
            &self.exec_output(format!("echo {artifact}"), true)?
        } else {
            artifact
        };
        // Expand path
        let path_expand = if path.starts_with("~") {
            &shellexpand::tilde_with_context(&path, || Some(&self.home)).to_string()
        } else if path.starts_with("/") {
            path
        } else if path.starts_with("./") {
            &path.replace("./", &format!("{}/", &self.workdir))
        } else {
            &format!("{}/{path}", &self.workdir)
        };
        // Build and run command
        let mut command = self.build_rsync_command()?;
        command.arg(format!(
            "{}@{}:{}",
            &self.server.user, &self.server.host, path_expand
        ));
        command.arg(current_dir.to_string_lossy().to_string());
        run_command(&mut command)?;
        Ok(())
    }

    /// Exec ssh command
    pub fn exec<T: std::fmt::Display>(&self, command: T) -> Result<()> {
        let mut command = self.build_ssh_command(command, SSHMode::StdOut);
        run_command(&mut command)?;
        Ok(())
    }

    /// Exec command and return output. Wrap into "echo '...' | bash" if necessary
    pub fn exec_output<T: std::fmt::Display>(&self, command: T, wrap: bool) -> Result<String> {
        let mode = if wrap { SSHMode::StdOut } else { SSHMode::Raw };
        let mut c = self.build_ssh_command(command, mode);
        log::debug!("Command to run: {:?}", &c);
        let output = c.output()?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Check pid on remote server
    pub fn check_pid(&self, pid: &String) -> Result<bool> {
        let remote_command = format!("if {}; then echo alive; else echo dead; fi", task_alive_check(pid));
        let alive = self.exec_output(remote_command, false)?.trim() == "alive";
        Ok(alive)
    }

    /// Gracefully kill remote pid
    pub fn kill(&self, pid: &String) -> Result<()> {
        self.exec(kill_tree_script(pid))?;
        Ok(())
    }

    /// Run action and save its pid in project state
    pub fn run(
        &self,
        project: &mut Project,
        command: RemoteCommand,
        sync: bool,
        args: Option<String>,
    ) -> Result<String> {
        Ok(self.run_tracked(project, command, sync, args)?.0)
    }

    /// Like run(), also returning the run id. Each run logs to `logs/runs/<name>-<run id>.log`;
    /// `logs/<name>.log` is a symlink to the latest run, so earlier runs' logs are kept.
    pub fn run_tracked(
        &self,
        project: &mut Project,
        command: RemoteCommand,
        sync: bool,
        args: Option<String>,
    ) -> Result<(String, String)> {
        // Load session and push
        let session = project
            .state
            .current_session_mut()
            .context("No active session. Start new session using \"yunpao session new\"")?;
        if sync {
            self.push()?;
        }
        // Define base command
        let (mut remote_command, action_name) = match command {
            RemoteCommand::Exec(cmd) => {
                let name = cmd.split(" ").next().unwrap().to_string();
                (cmd.clone(), name)
            }
            RemoteCommand::Named { name, command } => (command.clone(), name.clone()),
            RemoteCommand::Action(name) => (
                project
                    .actions
                    .get(name)
                    .context(format!("Action \"{name}\" is undefined"))?
                    .clone(),
                name.clone(),
            ),
        };
        let run_id = format!(
            "{}-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            &crate::utils::random_uuid()[..4]
        );
        let run_log = format!("runs/{action_name}-{run_id}.log");
        let logfile = format!("{}/{run_log}", &self.logdir);
        let link = format!("{}/{action_name}.log", &self.logdir);
        if let Some(args_str) = args
            && !args_str.trim().is_empty()
        {
            remote_command = format!("{remote_command} {}", args_str.trim());
        }
        // Quote properly so commands containing quotes or `$` survive both shells.
        let quote = |s: &str| -> Result<String> {
            Ok(shlex::try_quote(s).context("Command contains a NUL byte")?.into_owned())
        };
        let command_line = quote(&format!("[yunpao] Command: {remote_command}"))?;
        // The user's command runs in a subshell so `exit` / `set -e` inside it can't skip the
        // footer, and its exit code is captured right after it (`$?` after an echo is always 0).
        let script = format!(
            "echo \"[yunpao] Started: $(date +%Y-%m-%d\\ %H:%M:%S\\ %z)\"; printf '%s\\n' {command_line}; (\n{remote_command}\n)\nrc=$?; echo \"[yunpao] Finished: $(date +%Y-%m-%d\\ %H:%M:%S\\ %z)\"; echo \"[yunpao] Exit code: $rc\""
        );
        // `< /dev/null`: otherwise the task keeps ssh's stdin open and ssh waits for it to finish.
        // setsid: the task gets its own process group, so kill() can stop python & co., not just bash.
        // The braces keep `&` on nohup alone, so `$!` is the task's pid (= its process group).
        remote_command = format!(
            "mkdir -p {logdir}/runs && \
             {{ if [ -f {link} ] && [ ! -L {link} ]; then mv {link} {logdir}/runs/{action_name}-old-$(date +%s).log; fi; }} && \
             rm -f {link} && ln -s {run_log} {link} && \
             {{ nohup $(command -v setsid) bash -l -c {} < /dev/null > {logfile} 2>&1 & }}; echo $!",
            quote(&script)?,
            logdir = self.logdir,
        );
        // Save pid
        let pid = self.exec_output(remote_command, false)?;
        anyhow::ensure!(
            !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()),
            "Failed to start the task on the server (got {pid:?} instead of a process id)"
        );
        // Record the pid on top of the state as it is on disk now: the upload and the ssh above
        // can take minutes, and other submits / stops may have saved meanwhile.
        let snapshot = session.clone();
        let current = project.state.current_session().map(|s| s.id.clone());
        let (fresh, _) = crate::storage::ProjectState::update(&project.state.id().clone(), |st| {
            let s = st.session_or_insert(snapshot);
            s.tasks.insert(action_name, pid.clone());
            if let Some(c) = current {
                st.switch_current(c);
            }
            Ok(())
        })?;
        project.state = fresh;
        Ok((pid, run_id))
    }

    /// Run interactive ssh session
    pub fn ssh(&self) -> Result<()> {
        let mut command = self.build_ssh_command("bash", SSHMode::Interactive);
        run_interactive(&mut command)
    }

    /// Upload current workdir to remote server
    pub fn push(&self) -> Result<()> {
        let mut command = self.build_rsync_command()?;
        self.push_args(&mut command, None)?;
        let status = command.spawn()?.wait()?;
        check_rsync_status(status, b"")
    }

    /// Fetch artifacts from remote server
    pub fn pull(&self) -> impl Iterator<Item = (bool, &String)> {
        let results = self.artifacts.iter().map(|artifact| {
            if self.pull_artifact(artifact).is_ok() {
                (true, artifact)
            } else {
                (false, artifact)
            }
        });
        results
    }

    /// Check logs
    pub fn logs(&self, action: &String) -> Result<()> {
        let path = format!("{}/{action}.log", &self.logdir);
        let command = format!("cat {path} 2>/dev/null || echo 'No log file found'");
        self.exec(command)?;
        Ok(())
    }

    /// Watch logs
    pub fn watch(&self, action: &String) -> Result<()> {
        let path = format!("{}/{action}.log", &self.logdir);
        let command = format!("tail -f {path} 2>/dev/null || echo 'No log file found'");
        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .context("Error setting Ctrl-C handler")?;
        while running.load(Ordering::SeqCst) {
            // Ctrl-C kills the ssh child with a non-zero status; that's a detach, not an error.
            if let Err(e) = self.exec(&command)
                && running.load(Ordering::SeqCst)
            {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Remove session files on remote server
    pub fn clear(&self) -> Result<()> {
        let path = PathBuf::from(&self.workdir)
            .parent()
            .context("Invalid path")?
            .parent()
            .context("Invalid path")?
            .to_string_lossy()
            .to_string();
        let command = format!("rm -r {path}");
        self.exec(command)?;
        Ok(())
    }
}

/// Run `command` under a pseudo-terminal (util `script`) so its output isn't block-buffered.
/// Falls back to the plain command where `script` isn't available. Exit codes pass through.
fn with_pty(command: Command) -> Command {
    let program = command.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    if Command::new("script").arg("-V").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        return command;
    }
    let mut wrapped = Command::new("script");
    if cfg!(target_os = "linux") {
        // util-linux: script -q -e -c "<command line>" /dev/null
        let mut words = vec![program.as_str()];
        words.extend(args.iter().map(String::as_str));
        let Ok(line) = shlex::try_join(words) else { return command };
        wrapped.args(["-q", "-e", "-c", &line, "/dev/null"]);
    } else {
        // BSD / macOS: script -q /dev/null <program> <args…>
        wrapped.args(["-q", "/dev/null", &program]).args(&args);
    }
    wrapped
}

/// 23/24 = some files couldn't be read or vanished mid-transfer; the rest arrived.
fn check_rsync_status(status: std::process::ExitStatus, stderr: &[u8]) -> Result<()> {
    match status.code() {
        Some(0) => Ok(()),
        Some(23 | 24) => {
            log::warn!("Some files could not be uploaded (rsync exit {status})");
            Ok(())
        }
        _ => {
            let err = String::from_utf8_lossy(stderr);
            let last = err.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("**")).last().unwrap_or("");
            anyhow::bail!("rsync failed ({status}) {last}")
        }
    }
}

/// Shell test: the pid is alive AND is still one of our task wrappers. PIDs get reused after a
/// task ends; without the second check we'd report or kill some unrelated process.
pub fn task_alive_check(pid: &str) -> String {
    format!("kill -0 {pid} 2>/dev/null && ps -o args= -p {pid} 2>/dev/null | grep -q yunpao")
}

/// Shell snippet that stops a task and everything it started. Tasks launched with setsid
/// own a process group; older ones don't, so also signal children and grandchildren.
/// Every step tolerates failure: it runs under `set -e`.
pub fn kill_tree_script(pid: &str) -> String {
    format!(
        "if {alive}; then \
           kids=$(pgrep -P {pid} || true); grand=$(for k in $kids; do pgrep -P $k || true; done); \
           kill -TERM -- -{pid} 2>/dev/null || true; kill -TERM {pid} $kids $grand 2>/dev/null || true; \
         fi; true",
        alive = task_alive_check(pid)
    )
}

/// Run commands
fn run_command(command: &mut Command) -> Result<()> {
    log::debug!("Command to run: {:?}", command);
    let mut process = command
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let status = process.wait()?;
    anyhow::ensure!(
        status.success(),
        "{} failed ({status})",
        command.get_program().to_string_lossy()
    );
    Ok(())
}

/// Interactive sessions end with whatever the user's last command returned; don't treat that as failure.
fn run_interactive(command: &mut Command) -> Result<()> {
    command.spawn()?.wait()?;
    Ok(())
}

/// Start interactive ssh session to server
pub fn interactive_ssh(server: &Server) -> Result<()> {
    let mut command = Command::new("ssh");
    command.arg("-t");
    if let Some(identity) = server.identity.as_ref() {
        let identity_path = shellexpand::tilde(identity);
        command.arg("-i");
        command.arg(identity_path.to_string());
    }
    command.arg(format!("-p {}", &server.port));
    command.arg(format!(
        "{user}@{host}",
        user = &server.user,
        host = &server.host,
    ));
    command.arg("bash");
    run_interactive(&mut command)
}

/// Run ssh-copy-id for server
pub fn ssh_copy_id(server: &Server) -> Result<()> {
    let mut command = Command::new("ssh-copy-id");
    let identity = server
        .identity
        .as_ref()
        .context("Identity file is not specified in server config")?;

    let identity_path = shellexpand::tilde(identity);
    command.arg("-i");
    command.arg(identity_path.to_string());
    command.arg(format!("-p {}", &server.port));
    command.arg(format!(
        "{user}@{host}",
        user = &server.user,
        host = &server.host,
    ));
    run_command(&mut command)?;
    Ok(())
}

/// Clear server
pub fn clear_cache(server: &Server) -> Result<()> {
    let mut command = Command::new("ssh");
    if let Some(identity) = server.identity.as_ref() {
        let identity_path = shellexpand::tilde(identity);
        command.arg("-i");
        command.arg(identity_path.to_string());
    }
    command.arg(format!("-p {}", &server.port));
    command.arg(format!(
        "{user}@{host}",
        user = &server.user,
        host = &server.host,
    ));
    command.arg("rm -r ~/.cache/yunpao/sessions");
    run_command(&mut command)?;
    Ok(())
}

/// Reuse one connection per server instead of a fresh handshake for every command.
/// If the user's ssh config already multiplexes this host, defer to it; otherwise use our
/// own socket dir. Password servers work by riding on a master opened once (in a terminal,
/// or by the web UI's login form).
pub fn mux_opts(host: &str) -> Vec<String> {
    if user_controls(host) {
        return Vec::new();
    }
    let Some(dir) = control_dir() else { return Vec::new() };
    vec![
        "-o".into(), "ControlMaster=auto".into(),
        "-o".into(), format!("ControlPath={}/%C", dir.display()),
        "-o".into(), "ControlPersist=120".into(),
    ]
}

/// Whether ~/.ssh/config sets a ControlPath for this host (cached per host).
pub fn user_controls(host: &str) -> bool {
    use std::{collections::HashMap, sync::Mutex};
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, bool>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&v) = cache.lock().unwrap().get(host) {
        return v;
    }
    let v = !host.starts_with('-')
        && std::process::Command::new("ssh")
            .args(["-G", host])
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.strip_prefix("controlpath ").is_some_and(|p| p.trim() != "none"))
            })
            .unwrap_or(false);
    cache.lock().unwrap().insert(host.to_string(), v);
    v
}

pub fn control_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = dirs::cache_dir()?.join("yunpao").join("ssh");
        // Unix sockets max out at 104 bytes: "<dir>/" + 40-char %C + ssh's 17-char temp suffix.
        if dir.as_os_str().len() + 58 < 104 {
            std::fs::create_dir_all(&dir).ok()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
            }
            return Some(dir);
        }
        short_control_dir()
    })
    .clone()
}

/// A short socket dir in /tmp for long home paths. /tmp is shared, so the dir must be a real
/// directory owned by us, not something another user planted.
#[cfg(unix)]
fn short_control_dir() -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let uid = std::fs::metadata(dirs::home_dir()?).ok()?.uid();
    let dir = PathBuf::from(format!("/tmp/yunpao-{uid}"));
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    if !meta.is_dir() || meta.uid() != uid {
        return None;
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    Some(dir)
}

#[cfg(not(unix))]
fn short_control_dir() -> Option<PathBuf> {
    None
}

/// A project folder that would sync far too much: the home directory (all your files,
/// SSH keys included) or anything above it. Returns why, for the error message.
pub fn too_broad_project_dir(dir: &std::path::Path) -> Option<String> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let home = dirs::home_dir().map(|h| h.canonicalize().unwrap_or(h))?;
    (home.starts_with(&dir)).then(|| {
        format!(
            "项目文件夹是 {}（主目录或更上层），提交会把里面所有文件（包括 ~/.ssh 私钥）上传到服务器，已拦下。\
             请先 cd 到你的项目文件夹（比如 ~/NSEC/voice-demo）再运行 yunpao。",
            dir.display()
        )
    })
}

/// The server's home directory, asked once per server (`$HOME`) and cached; lab servers often
/// keep homes under /data or /mnt. Falls back to /home/<user> (or /root) when the server can't
/// be asked right now (offline, not logged in) — then it is asked again a minute later.
pub fn remote_home(server: &Server) -> String {
    use std::time::{Duration, Instant};
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, (Option<String>, Instant)>>> = std::sync::OnceLock::new();
    let fallback = if server.user == "root" { "/root".to_string() } else { format!("/home/{}", server.user) };
    let key = format!("{}@{}:{}", server.user, server.host, server.port);
    let cache = CACHE.get_or_init(Default::default);
    if let Some((home, at)) = cache.lock().unwrap().get(&key) {
        match home {
            Some(h) => return h.clone(),
            None if at.elapsed() < Duration::from_secs(60) => return fallback,
            None => {}
        }
    }
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]).args(mux_opts(&server.host)).arg("-p").arg(server.port.to_string());
    if let Some(id) = &server.identity {
        cmd.arg("-i").arg(shellexpand::tilde(id).into_owned());
    }
    cmd.arg(format!("{}@{}", server.user, server.host)).arg("printf %s \"$HOME\"").stdin(Stdio::null());
    // It goes into shell lines unquoted: accept plain absolute paths only.
    let home = cmd
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| h.len() > 1 && h.starts_with('/') && h.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)));
    cache.lock().unwrap().insert(key, (home.clone(), Instant::now()));
    home.unwrap_or(fallback)
}

/// A file name that usually holds a secret: SSH / TLS keys, .env files, cloud credentials.
pub fn looks_secret(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let in_dir = |d: &str| path.split('/').any(|seg| seg == d);
    name == ".env" || name.starts_with(".env.")
        || ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"].iter().any(|k| name.starts_with(k))
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks", ".ovpn"].iter().any(|e| name.ends_with(e))
        || (name.starts_with("credentials") || name.contains("service_account") || name.contains("secret")) && name.ends_with(".json")
        || [".netrc", ".pgpass", ".git-credentials", ".npmrc", ".pypirc"].contains(&name.as_str())
        || in_dir(".ssh") || in_dir(".aws") || in_dir(".gnupg")
}

/// A catch-all folder (Downloads, Desktop, Documents…) used as a project: everything in it
/// would be uploaded. Allowed, but worth a warning.
pub fn junk_project_dir(dir: &std::path::Path) -> Option<String> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let home = dirs::home_dir()?;
    let home = home.canonicalize().unwrap_or(home);
    let rel = dir.strip_prefix(&home).ok()?;
    let mut parts = rel.components();
    let first = parts.next()?.as_os_str().to_string_lossy().to_string();
    let junk = ["Downloads", "Desktop", "Documents", "Library", "Movies", "Music", "Pictures", "下载", "桌面", "文稿"];
    (parts.next().is_none() && junk.contains(&first.as_str())).then(|| {
        format!("项目文件夹是 ~/{first}，里面所有文件都会被上传。建议新建一个只放这个项目的文件夹。")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_secret_files() {
        for p in [".env", "config/.env.local", "keys/id_rsa", "server.pem", "gcp/credentials.json", "a/.ssh/config", "my-secret.json"] {
            assert!(looks_secret(p), "{p}");
        }
        for p in ["train.py", "data/env.csv", "results/key_points.npy", "README.md", "secrets.py"] {
            assert!(!looks_secret(p), "{p}");
        }
    }
}
