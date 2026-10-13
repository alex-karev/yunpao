---
name: yunpao
description: Remote session and task manager. Use when the user needs to deploy code, run commands on remote servers, manage SSH servers/sessions, or work with yunpao CLI.
---

# Yunpao

Yunpao is a Rust CLI for remote dev workflows over SSH. It syncs local project files to a remote server, runs predefined or ad-hoc shell commands as background tasks, streams logs, and downloads artifacts back.

## Core concepts

- **Project** — a local directory with `yunpao.toml` (actions, artifacts, dotenv toggle).
- **Server** — SSH target stored in global config (`~/.config/yunpao/config.toml`).
- **Session** — binds a project to a server. Multiple sessions per project.
- **Action** — a shell command defined in `yunpao.toml` under `[actions]`.
- **Task** — a running action or exec command tracked by PID on the remote.

## Remote layout

```
~/.cache/yunpao/sessions/<session_id>/
  working/   # rsynced project files
  logs/      # <action>.log files
```

Env vars `YUNPAO_WORKDIR` and `YUNPAO_LOGDIR` are injected into all remote commands.

## Non-interactive mode

Most commands that prompt for confirmation accept `-y` / `--yes` to skip dialogs. Use this flag when scripting or when the user clearly wants to proceed without prompts.

## CLI reference

### Core workflow

- `yunpao init` — create `yunpao.toml` in current dir. Prompts to edit unless `-y`.
- `yunpao push` — rsync current directory to remote `working/`.
- `yunpao pull` — download `artifacts` from remote to local.
- `yunpao run <action> [-w] [-f] [--no-sync] [args...]` — run a predefined action as a background task.
  - `-w` / `--watch` — tail logs immediately after start.
  - `-f` / `--force` — override if same action already running.
  - `--no-sync` — skip the push step.
  - trailing args are passed to the action command.
- `yunpao logs <action> [-w]` — print or tail logs.
- `yunpao watch <action>` — alias for `logs --watch`.
- `yunpao kill <action>` — kill running action by name.
- `yunpao exec <command> [-w] [-f] [--no-sync] [args...]` — run arbitrary command as background task. First word becomes the action id for `kill`/`logs`.
- `yunpao ssh` — interactive SSH to current session's server.
- `yunpao clear [-y]` — delete session cache on remote.
- `yunpao edit [-g]` — open `yunpao.toml` or global config in editor.

### Server management

- `yunpao server new [host] [-a alias]... [-i identity] [-e env]... [-y]` — add server. Host format: `user@host:port` or `user@host` (default port 22).
- `yunpao server list` — list servers.
- `yunpao server delete <name> [-y]` — remove by id or alias.
- `yunpao server clear <name> [-y]` — delete all yunpao cache on server.
- `yunpao server ssh <name>` — SSH to server.
- `yunpao server copy-id <name>` — `ssh-copy-id` using configured identity.

### Session management

- `yunpao session new [name] [-s server] [-y]` — create session. Prompts for server if not given.
- `yunpao session list` — list project sessions.
- `yunpao session switch <name>` — switch active session.
- `yunpao session delete <name> [-y]` — delete session.

## How to help

- Check if `yunpao.toml` exists before suggesting project commands. If missing, suggest `yunpao init`.
- Check if servers exist before suggesting remote commands. If missing, suggest `yunpao server new`.
- Check if an active session exists. If missing, suggest `yunpao session new`.
- Prefer `yunpao run <action>` over `yunpao exec` when the action is defined in `yunpao.toml`.
- Use `-y` when the user clearly wants non-interactive behavior.
- Use `-w` when the user wants to see output immediately.
- Use `-f` when overriding a running task.
- Use `--no-sync` when files haven't changed since last push.
- For pulling artifacts, verify `yunpao.toml` has the right `artifacts` paths.
- Read `yunpao.toml` directly when you need to know project-specific actions or artifacts.
