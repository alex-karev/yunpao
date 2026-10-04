use crate::cli::args::{Args, Commands, ServerCommands, SessionCommands};
use crate::cli::helpers::*;
use crate::cli::logger::{CliclackLogger, CliclackTheme};
use crate::cli::server_dialogs::{new_server_dialog, select_server_dialog};
use crate::cli::session_dialogs::{new_session_name_dialog, select_session_dialog};
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use cliclack::{confirm, intro, log::success, outro, outro_cancel, set_theme};
use colored::Colorize;
use log::LevelFilter;
use yunpao::config::{Global, Project};
use yunpao::ssh::{RemoteContext, clear_cache, interactive_ssh, ssh_copy_id};

/// Start CLI application
pub fn run() -> Result<()> {
    // Set logging
    set_theme(CliclackTheme);
    let _args = Args::parse();
    let log_level = match _args.verbose {
        0 => LevelFilter::Error,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };
    log::set_logger(&CliclackLogger)
        .map(|()| log::set_max_level(log_level))
        .unwrap();

    // Load data
    let mut config = Global::load()?;
    log::debug!("Config: {:?}", &config);

    // Parse args
    match _args.command {
        /*
         * Core Features
         */
        Commands::Init => {
            println!(include_str!("../art.txt"));
            ensure!(
                !Project::exists(),
                "Project is already initialized. See yunpao.toml"
            );
            Project::init()?;
            success("Project initialized")?;
            if confirm("Would you like to edit yunpao.toml now?").interact()? {
                edit::edit_file(Project::get_path()?)?;
            }
            outro("Done")?;
        }

        Commands::Push => {
            intro("Uploading files to the remote")?;
            let project = get_project()?;
            let context = RemoteContext::new(&config, &project)?;
            context.push()?;
            outro("Done!")?;
        }

        Commands::Pull => {
            intro("Fetching artifacts from the remote")?;
            let project = get_project()?;
            let context = RemoteContext::new(&config, &project)?;
            for (status, artifact) in context.pull() {
                if status {
                    log::info!("Pulled '{artifact}'");
                } else {
                    log::error!("Error pulling '{artifact}'");
                }
            }
            outro("Done!")?;
        }

        Commands::Run {
            action,
            watch,
            force,
            no_sync,
        } => {
            intro(format!("Running \"{action}\""))?;
            let mut project = get_project()?;
            let context = RemoteContext::new(&config, &project)?;
            if no_sync {
                log::warn!("--no-sync flag passed. Skipping upload operation.");
            }
            let session = get_session_mut(&mut project)?;
            if !force
                && session.tasks.contains_key(&action)
                && context.check_pid(session.tasks.get(&action).unwrap())?
                && !confirm(format!(
                    "Action \"{action}\" might be still running on remote (PID {}), run anyway?",
                    session.tasks.get(&action).unwrap()
                ))
                .interact()?
            {
                bail!("Action cancelled!");
            }
            let pid = context.run(&mut project, &action, !no_sync)?;
            success(format!("Process is started with pid {pid}"))?;
            if watch {
                watch_logs(&context, &action)?;
            } else {
                outro("Done!")?;
            }
        }

        Commands::Logs { action, watch } => {
            intro(format!("Reading logs for \"{action}\""))?;
            let project = get_project()?;
            let context = RemoteContext::new(&config, &project)?;
            if watch {
                watch_logs(&context, &action)?;
            } else {
                log::info!("Logs");
                context.logs(&action)?;
                outro("End")?;
            }
        }

        Commands::Watch { action } => {
            intro(format!("Watching \"{action}\""))?;
            let project = get_project()?;
            let context = RemoteContext::new(&config, &project)?;
            watch_logs(&context, &action)?;
        }

        Commands::Kill { action } => {
            intro(format!("Stopping \"{action}\" action").red())?;
            let mut project = get_project()?;
            let context = RemoteContext::new(&config, &project)?;
            let session = get_session_mut(&mut project)?;
            let pid = session
                .tasks
                .get(&action)
                .context(format!("Task \"{action}\" is not running on remote"))?;
            if context.check_pid(pid)? {
                log::info!("Killing pid {pid}");
                context.kill(pid)?;
                session.tasks.remove(&action);
                project.state.save()?;
                outro("Done!")?;
            } else {
                outro_cancel("Task is not running")?;
            }
        }

        Commands::Exec { command } => {
            intro(format!("Running \"{command}\""))?;
            let project = get_project()?;
            let context = RemoteContext::new(&config, &project)?;
            context.exec(command)?;
            outro("Done!")?;
        }

        Commands::Clear { yes } => {
            intro("Deleting session cache".red())?;
            if yes
                || confirm("Are you sure you want to delete this session's cache on remote server?")
                    .interact()?
            {
                let project = get_project()?;
                let context = RemoteContext::new(&config, &project)?;
                context.clear()?;
                outro("Done!")?;
            } else {
                outro_cancel("Action cancelled")?;
            }
        }

        Commands::SSH => {
            intro("Interactive SSH Session")?;
            let project = get_project()?;
            let context = RemoteContext::new(&config, &project)?;
            context.ssh()?;
            outro("Disconnected")?;
        }

        Commands::Edit { global } => {
            if global {
                intro("Editing global config")?;
                if !Global::exists() {
                    config.save()?;
                }
                edit::edit_file(Global::get_path()?)?;
            } else {
                intro("Editing project config")?;
                let _ = get_project()?;
                edit::edit_file(Project::get_path()?)?;
            }
            outro("Done!")?;
        }

        /*
         * Server Managment
         */
        Commands::Server { command } => match command {
            ServerCommands::List => {
                intro("Servers")?;
                let mut n_servers = 0;
                for server in config.servers.values() {
                    n_servers += 1;
                    println!("{}: {}", server.id.bold().blue(), server.aliases.join(", "));
                }
                outro(format!("{}: {n_servers} servers", "Total".blue().bold()))?;
            }

            ServerCommands::New {
                host,
                alias,
                identity,
                env,
                yes,
            } => {
                intro("New Server")?;
                let server = new_server_dialog(host, alias, identity, env, yes)?;
                config.add_server(server)?;
                log::info!("Global config updated!");
                outro("Done!")?;
            }

            ServerCommands::Delete { name, yes } => {
                intro("Delete server".red())?;
                let server_id = {
                    let server = select_server_dialog(&config, name)?;
                    if yes
                        || confirm(format!(
                            "Are you sure you want to delete server \"{}\" ({})?",
                            server.aliases.join(", "),
                            server.id
                        ))
                        .interact()?
                    {
                        Some(server.id.clone())
                    } else {
                        None
                    }
                };
                if let Some(id) = server_id {
                    config.delete_server(&id)?;
                    log::info!("Global config updated!");
                    outro("Done!")?;
                } else {
                    outro_cancel("Action cancelled")?;
                }
            }

            ServerCommands::Clear { name, yes } => {
                intro("Deleting server cache".red())?;
                let query = get_session_server(name);
                let server = select_server_dialog(&config, query)?;
                if yes
                    || confirm(format!(
                        "Are you sure you want to delete ALL cache on server \"{}\" ({})?",
                        server.aliases.join(", "),
                        server.id
                    ))
                    .interact()?
                {
                    clear_cache(&server)?;
                    outro("Done!")?;
                } else {
                    outro_cancel("Action cancelled")?;
                }
            }

            ServerCommands::SSH { name } => {
                intro("Interactive SSH Session")?;
                let query = get_session_server(name);
                let server = select_server_dialog(&config, query)?;
                interactive_ssh(&server)?;
                outro("Disconnected")?;
            }

            ServerCommands::CopyId { name } => {
                intro("Uploading SSH identity")?;
                let query = get_session_server(name);
                let server = select_server_dialog(&config, query)?;
                ssh_copy_id(&server)?;
                outro("Done!")?;
            }
        },

        /*
         * Session Managment
         */
        Commands::Session { command } => {
            let project = get_project()?;
            let mut state = project.state;
            match command {
                SessionCommands::List => {
                    intro("Servers")?;
                    let mut n_sessions = 0;
                    let current_session = state.current_session();
                    for session in state.list_sessions() {
                        n_sessions += 1;
                        let is_current =
                            current_session.is_some() && current_session.unwrap().id == session.id;
                        let label = if is_current { " [current]" } else { "" };
                        println!(
                            "{}: {}{} [{}]",
                            session.id.bold().blue(),
                            session.name,
                            label.bold().green(),
                            session.created_at_str().italic().dimmed()
                        );
                    }
                    outro(format!("{}: {n_sessions} sessions", "Total".blue().bold()))?;
                }

                SessionCommands::New { name, server, yes } => {
                    intro("New Session")?;
                    let name = new_session_name_dialog(name, yes)?;
                    let server_id = if !yes && server.is_none() {
                        select_server_dialog(&config, server)?.id.clone()
                    } else {
                        config.find_server(&server.unwrap())?.id.clone()
                    };
                    state.new_session(&server_id, name.as_ref())?;
                    let current_session = state.current_session().unwrap();
                    log::info!("State file updated!");
                    log::info!("Switched to session \"{}\"", current_session.name.bold());
                    outro("Done!")?;
                }

                SessionCommands::Switch { name } => {
                    intro("Switching session")?;
                    let session_id = select_session_dialog(&state, name)?.id.clone();
                    state.switch_session(&session_id)?;
                    let session = state.current_session().unwrap();
                    outro(format!(
                        "Session switched to \"{} [{}]\"",
                        session.name.bold(),
                        session.created_at_str().dimmed()
                    ))?;
                }

                SessionCommands::Delete { name, yes } => {
                    intro("Deleting session".red())?;
                    let session_id = {
                        let session = select_session_dialog(&state, name)?;
                        if yes
                            || confirm(format!(
                                "Are you sure you want to delete session \"{}\"?",
                                session.name
                            ))
                            .interact()?
                        {
                            Some(session.id.clone())
                        } else {
                            None
                        }
                    };
                    if let Some(id) = session_id {
                        state.delete_session(&id)?;
                        log::info!("State file updated!");
                        outro("Done!")?;
                    } else {
                        outro_cancel("Action cancelled")?;
                    }
                }
            }
        }
    }
    Ok(())
}
