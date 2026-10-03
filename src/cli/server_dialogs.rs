use anyhow::{Result, ensure};
use cliclack::{confirm, input, log, select};
use std::path::PathBuf;
use yunpao::config::{Global, HostArgs, Server};

/// Displays identities from ~/.ssh and offers to pick one
pub fn identity_dialog() -> anyhow::Result<Option<PathBuf>> {
    let mut ssh_dir = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home dir"))?;
    ssh_dir.push(".ssh");
    if !ssh_dir.exists() {
        log::error(format!(
            "'{}' does no exists! Skipping identity dialog.",
            &ssh_dir.to_str().unwrap()
        ))?;
        return Ok(None);
    }
    let files: Vec<PathBuf> = std::fs::read_dir(ssh_dir)?
        .filter_map(|x| {
            if x.is_err() {
                return None;
            }
            let path = x.unwrap().path();
            if path.is_dir() {
                return None;
            }
            if let Some(basename) = path.file_name() {
                let basename = basename.to_str().unwrap_or("config");
                if basename == "config" || basename.contains(".") || basename == "known_hosts" {
                    return None;
                }
            }
            Some(path)
        })
        .collect();
    if files.len() == 0 {
        log::error("No identities found. Skipping identity dialog")?;
        return Ok(None);
    }
    let mut identity_dialog = select("Choose the identity to use");
    for file in files.iter() {
        let basename = file.file_name().unwrap().to_str().unwrap();
        identity_dialog = identity_dialog.item(file, basename, "");
    }
    let identity = identity_dialog.interact()?.clone();
    Ok(Some(identity))
}

/// New server dialog
pub fn new_server_dialog(
    host: Option<String>,
    mut alias: Vec<String>,
    identity: Option<String>,
    env: Vec<String>,
    non_interactive: bool,
) -> Result<Server> {
    // Parse host
    ensure!(
        !(non_interactive && host.is_none()),
        "Host should be specified in non-interactive mode."
    );
    let host = host.unwrap_or_else(|| {
        input("Hostname")
            .placeholder("example.com, root@example.com, ssh root@example.com -p 22, etc.")
            .interact()
            .unwrap()
    });
    let mut server = Server::new(HostArgs::FromString { string: &host }, identity)?;
    // Parse env
    let parse_env = |item: &String| -> Result<Option<(String, String)>> {
        let mut parts = item.split("=");
        let key = parts.next();
        let val = parts.next();
        if key.is_none() || val.is_none() {
            log::error(format!("Invalid env format: {}", item))?;
            Ok(None)
        } else {
            Ok(Some((key.unwrap().to_string(), val.unwrap().to_string())))
        }
    };
    for item in env {
        if let Some((key, val)) = parse_env(&item)? {
            server.env.insert(key, val);
        }
    }
    // Set identity
    if server.identity.is_none() {
        if non_interactive {
            log::error(
                "Identity file is not specified, it will be handled automatically by your ssh config",
            )?;
        } else {
            server.identity = Some(
                identity_dialog()?
                    .expect("No SSH identity selected")
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }
    // Add aliases
    if !non_interactive && confirm("Would you like to add an alias for this server?").interact()? {
        let new_alias = input("Enter alias").placeholder("myserver").interact()?;
        alias.push(new_alias);
    }
    for item in alias.iter() {
        server.aliases.push(item.clone());
    }
    // Add env
    while !non_interactive
        && confirm("Would you like to add a server-specific environment variable?").interact()?
    {
        let new_env: String = input("Enter env").placeholder("FOO=BAR").interact()?;
        if let Some((key, val)) = parse_env(&new_env)? {
            server.env.insert(key, val);
        }
    }
    Ok(server)
}

/// Select server name dialog
pub fn select_server_dialog<'a>(config: &'a Global, query: Option<String>) -> Result<&'a Server> {
    let server_query = if query.is_none() {
        let mut server_dialog = select("Choose server");
        for (key, value) in config.servers.iter() {
            server_dialog = server_dialog.item(
                key,
                key,
                format!(
                    "{user}@{host}:{port}",
                    user = value.user,
                    host = value.host,
                    port = value.port,
                ),
            );
        }
        if let Some(default_server) = config.default_server.as_ref() {
            server_dialog = server_dialog.initial_value(default_server);
        }
        server_dialog.interact()?
    } else {
        &query.unwrap()
    };
    let server = config.find_server(server_query)?;
    Ok(server)
}
