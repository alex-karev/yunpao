use crate::config::{Global, Project, Server};
use anyhow::{Context, Result};
use dotenvy::dotenv_iter;
use log;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
}

#[derive(Debug)]
pub struct RemoteContext<'a> {
    workdir: String,
    logdir: String,
    home: String,
    server: &'a Server,
    artifacts: Vec<String>,
    env: HashMap<String, String>,
}

impl<'a> RemoteContext<'a> {
    /// Build new remote context
    pub fn new(config: &'a Global, project: &Project) -> Result<Self> {
        // Get session
        let session = project
            .state
            .current_session()
            .context("No active session. Start new session using \"yunpao session new\"")?;
        // Get server
        let server = config.find_server(&session.server)?;
        // Resolve paths
        let home = if server.user == "root" {
            "/root".to_string()
        } else {
            format!("/home/{}", &server.user).to_string()
        };
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
        })
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
        // Define dirs
        let current_dir = std::env::current_dir()?;
        let mut gitdir = current_dir.clone();
        gitdir.push(".git");
        // Define ssh base command
        let mut ssh_command = format!("ssh -p {}", &self.server.port);
        if let Some(identity) = self.server.identity.as_ref() {
            ssh_command = format!("{} -i {}", ssh_command, identity);
        }
        // Build command
        let mut command = Command::new("rsync");
        command.arg("-azP");
        command.arg("--mkpath");
        command.arg("--out-format=%n%L");
        command.arg("-e");
        command.arg(ssh_command);
        if gitdir.exists() {
            command.arg("-f");
            command.arg("- /.git/");
        }
        Ok(command)
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
        let remote_command = format!("kill -0 {pid} 2>/dev/null && echo alive || echo dead");
        let alive = self.exec_output(remote_command, false)?.trim() == "alive";
        Ok(alive)
    }

    /// Gracefully kill remote pid
    pub fn kill(&self, pid: &String) -> Result<()> {
        self.exec(format!("kill -TERM {pid}"))?;
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
            RemoteCommand::Action(name) => (
                project
                    .actions
                    .get(name)
                    .context(format!("Action \"{name}\" is undefined"))?
                    .clone(),
                name.clone(),
            ),
        };
        let logfile = format!("{}/{action_name}.log", &self.logdir);
        if let Some(args_str) = args
            && !args_str.trim().is_empty()
        {
            remote_command = format!("{remote_command} {}", args_str.trim());
        }
        let remote_command_log = remote_command.clone();
        // Wrap command
        remote_command = format!(
            "echo \"[yunpao] Started: $(date +%Y-%m-%d\\ %H:%M:%S)\"; echo \"[yunpao] Command: {remote_command_log}\"; {remote_command}; echo \"[yunpao] Finished: $(date +%Y-%m-%d\\ %H:%M:%S)\"; echo \"[yunpao] Exit code: $?\""
        );
        remote_command = format!("nohup bash -l -c '{remote_command}' > {logfile} 2>&1 & echo $!");
        // Save pid
        let pid = self.exec_output(remote_command, false)?;
        session.tasks.insert(action_name, pid.clone());
        project.state.save()?;
        Ok(pid)
    }

    /// Run interactive ssh session
    pub fn ssh(&self) -> Result<()> {
        let mut command = self.build_ssh_command("bash", SSHMode::Interactive);
        run_command(&mut command)?;
        Ok(())
    }

    /// Upload current workdir to remote server
    pub fn push(&self) -> Result<()> {
        let current_dir = std::env::current_dir()?;
        let mut gitignore = current_dir.clone();
        gitignore.push(".gitignore");
        let mut command = self.build_rsync_command()?;
        if gitignore.exists() {
            command.arg(format!(
                "--exclude-from={}",
                gitignore.to_string_lossy().to_string()
            ));
        }
        command.arg(format!("{}/", current_dir.to_string_lossy().to_string()));
        command.arg(format!(
            "{}@{}:{}",
            &self.server.user, &self.server.host, &self.workdir
        ));
        run_command(&mut command)?;
        Ok(())
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
        self.exec(command)?;
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

/// Run commands
fn run_command(command: &mut Command) -> Result<()> {
    log::debug!("Command to run: {:?}", command);
    let mut process = command
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    process.wait()?;
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
    run_command(&mut command)?;
    Ok(())
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
