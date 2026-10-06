use anyhow::{Context, Result, ensure};
use cliclack::{confirm, outro};
use colored::Colorize;
use yunpao::config::Project;
use yunpao::ssh::RemoteContext;
use yunpao::storage::Session;

pub fn get_project() -> Result<Project> {
    ensure!(
        Project::exists(),
        "Project is not initialized. Run \"yunpao init\" first"
    );
    Ok(Project::load()?)
}

pub fn get_session<'a>(project: &'a Project) -> Result<&'a Session> {
    project
        .state
        .current_session()
        .context("No active session. Start new session using \"yunpao session new\"")
}

pub fn get_session_mut<'a>(project: &'a mut Project) -> Result<&'a mut Session> {
    project
        .state
        .current_session_mut()
        .context("No active session. Start new session using \"yunpao session new\"")
}

pub fn get_session_server(name: Option<String>) -> Option<String> {
    if name.is_none()
        && let Some(project) = get_project().ok()
        && let Some(session) = get_session(&project).ok()
    {
        Some(session.server.clone())
    } else {
        name
    }
}

pub fn watch_logs(context: &RemoteContext, action: &String) -> Result<()> {
    log::info!("Watching log file (press {} to detach)", "Ctrl+C".bold().blue());
    context.watch(&action)?;
    println!("\n");
    log::info!("Detached. Use \"{} {}\" to re-attach.", "yunpao watch".bold().blue(), &action.bold().blue());
    outro("End")?;
    Ok(())
}

pub fn confirm_same_action(
    session: &Session,
    context: &RemoteContext,
    action: &str,
) -> Result<bool> {
    if let Some(pid) = session.tasks.get(action)
        && context.check_pid(pid)?
    {
        Ok(confirm(format!(
            "Action \"{action}\" might be still running on remote (PID {}), run anyway?",
            session.tasks.get(action).unwrap()
        ))
        .interact()?)
    } else {
        Ok(true)
    }
}

pub fn parse_args(args: Vec<String>) -> Option<String> {
    if args.len() > 0 {
        Some(args.join(" "))
    } else {
        None
    }
}
