use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A concrete host from ~/.ssh/config, resolved through `ssh -G`.
#[derive(Debug, Clone, Serialize)]
pub struct SshHost {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
}

fn ssh_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ssh"))
}

/// Aliases declared with `Host` in ~/.ssh/config (following `Include`), skipping
/// wildcard and negated patterns, in file order without duplicates.
pub fn list_aliases() -> Vec<String> {
    let Some(dir) = ssh_dir() else { return vec![] };
    let mut out = Vec::new();
    collect_aliases(&dir.join("config"), &dir, &mut out, 0);
    out
}

fn collect_aliases(file: &Path, ssh_dir: &Path, out: &mut Vec<String>, depth: u8) {
    if depth > 8 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(file) else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, rest) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, r)) => (k, r.trim_start_matches(|c: char| c.is_whitespace() || c == '=')),
            None => continue,
        };
        if key.eq_ignore_ascii_case("host") {
            for pat in rest.split_whitespace() {
                let pat = pat.trim_matches('"');
                if pat.contains(['*', '?']) || pat.starts_with('!') || pat.starts_with('-') {
                    continue;
                }
                if !out.iter().any(|a| a == pat) {
                    out.push(pat.to_string());
                }
            }
        } else if key.eq_ignore_ascii_case("include") {
            for inc in rest.split_whitespace() {
                for path in expand_include(inc.trim_matches('"'), ssh_dir) {
                    collect_aliases(&path, ssh_dir, out, depth + 1);
                }
            }
        }
    }
}

/// Resolve an Include argument: `~` expansion, relative to ~/.ssh, and a `*` in the file name.
fn expand_include(pattern: &str, ssh_dir: &Path) -> Vec<PathBuf> {
    let expanded = shellexpand::tilde(pattern).into_owned();
    let path = if Path::new(&expanded).is_absolute() {
        PathBuf::from(expanded)
    } else {
        ssh_dir.join(expanded)
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    let Some((prefix, suffix)) = name.split_once('*') else {
        return vec![path];
    };
    let Some(parent) = path.parent() else { return vec![] };
    let mut files: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.len() >= prefix.len() + suffix.len() && n.starts_with(prefix) && n.ends_with(suffix)
                })
        })
        .collect();
    files.sort();
    files
}

/// Ask ssh itself for the effective settings of an alias.
pub fn resolve(alias: &str) -> Option<SshHost> {
    if alias.starts_with('-') || alias.contains(char::is_whitespace) {
        return None;
    }
    let out = Command::new("ssh").args(["-G", alias]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ').map(str::to_string))
    };
    Some(SshHost {
        alias: alias.to_string(),
        hostname: field("hostname").unwrap_or_else(|| alias.to_string()),
        user: field("user").unwrap_or_default(),
        port: field("port").and_then(|p| p.parse().ok()).unwrap_or(22),
    })
}

pub fn list_hosts() -> Vec<SshHost> {
    list_aliases().iter().filter_map(|a| resolve(a)).collect()
}
