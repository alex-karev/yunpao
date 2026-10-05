use clap::{Parser, Subcommand};

/// Program arguments
#[derive(Parser, Debug)]
#[command(name = "Yunpao")]
#[command(version = "1.0")]
#[command(about = "Remote Session and Task Manager")]
#[command(long_about = None)] // TODO: Write long about
pub struct Args {
    /// Enable verbose logging
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Subcommands
    #[command(subcommand)]
    pub command: Commands,
}

/// Subcommands
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize project (yunpao.toml) in the current dir
    Init,
    /// Send current project files to remote server
    Push,
    /// Download artifacts from remote server
    Pull,
    /// Run action on remote server
    Run {
        /// Action to perform (configured in "yunpao.toml")
        action: String,
        /// Continuously sync logs right after start
        #[arg(short, long)]
        watch: bool,
        /// Force override if action is already running
        #[arg(short, long)]
        force: bool,
        /// Skip uploading files to remote (requires "yunpao push")
        #[arg(long)]
        no_sync: bool,
        /// Arguments to pass to the action
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Sync action logs from remote server
    Logs {
        /// Action to monitor
        action: String,
        /// Continuously sync logs
        #[arg(short, long)]
        watch: bool,
    },
    /// Shortcut for logs --watch
    Watch {
        /// Action to monitor
        action: String,
    },
    /// Kill action running on remote server
    Kill {
        /// Action to kill
        action: String,
    },
    /// Execute custom command on remote server
    Exec {
        /// Command to be executed
        command: String,
        /// Continuously sync logs right after start
        #[arg(short, long)]
        watch: bool,
        /// Force override if action is already running
        #[arg(short, long)]
        force: bool,
        /// Skip uploading files to remote (requires "yunpao push")
        #[arg(long)]
        no_sync: bool,
        /// Arguments to pass to the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Connect to remote server via ssh
    SSH,
    /// Delete session files on remote server
    Clear {
        /// Auto-confirm
        #[arg(short, long)]
        yes: bool,
    },
    /// Edit configuration file
    Edit {
        /// Edit global configuration
        #[arg(short, long)]
        global: bool,
    },
    /// Manage servers
    Server {
        #[command(subcommand)]
        command: ServerCommands,
    },
    /// Manage sessions
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServerCommands {
    /// Add new server
    New {
        /// Server hostname (example.com|root@example.com|root@example.com:22)
        host: Option<String>,
        /// Aliases
        #[arg(short, long, action = clap::ArgAction::Append)]
        alias: Vec<String>,
        /// SSH identity
        #[arg(short, long)]
        identity: Option<String>,
        /// Environment variables
        #[arg(short, long, action = clap::ArgAction::Append)]
        env: Vec<String>,
        /// Auto-confirm
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete server
    Delete {
        /// Server id or alias
        name: Option<String>,
        /// Auto-confirm
        #[arg(short, long)]
        yes: bool,
    },
    /// List servers
    List,
    /// Delete yunpao cache on remote server
    Clear {
        /// Server id or alias
        name: Option<String>,
        /// Auto-confirm
        #[arg(short, long)]
        yes: bool,
    },
    /// Open interactive ssh session
    SSH {
        /// Server id or alias (default: current session)
        name: Option<String>,
    },
    /// Copy ssh id to the server
    CopyId {
        /// Server id or alias (default: current session)
        name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionCommands {
    /// Start new session
    New {
        /// Session name (default: [date_created])
        name: Option<String>,
        /// Server id or alias
        #[arg(short, long)]
        server: Option<String>,
        /// Auto-confirm
        #[arg(short, long)]
        yes: bool,
    },
    /// Switch to a different session
    Switch {
        /// Session name or id
        name: Option<String>,
    },
    /// Delete session
    Delete {
        /// Session name or id
        name: Option<String>,
        /// Auto-confirm
        #[arg(short, long)]
        yes: bool,
    },
    /// List sessions
    List,
}
