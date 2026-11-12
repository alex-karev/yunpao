use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

const ABOUT: &str = "Yunpao is a CLI tool that syncs project files to remote Linux servers and runs long-running tasks in the background — ideal for ML training, remote builds, and large data processing jobs.";

const COMMANDS: &str = r#"Available yunpao commands:
  yunpao push                          Upload local project files to the remote server
  yunpao pull                           Download artifacts from the remote server
  yunpao run <action> [--watch]         Run a named action defined in yunpao.toml; --watch tails logs live
  yunpao exec "<shell command>" [--watch]  Run an arbitrary shell command on the remote server
  yunpao logs <action>                  View stored logs for an action
  yunpao watch <action>                 Tail action logs live (Ctrl-C to detach)
  yunpao kill <action>                  Stop a running action
  yunpao ssh                            Open an interactive SSH session using the active session's server
  yunpao server new --host <host>       Register a new server (user@host or user@host:port)
  yunpao server list                    List all registered servers
  yunpao server ssh [name]              SSH directly into a named server
  yunpao server delete [name]           Remove a registered server
  yunpao session new [--server <name>]  Create a new work session
  yunpao session list                   List all sessions
  yunpao session switch [name]          Switch the active session
  yunpao session delete [name]          Delete a session
  yunpao clear                          Delete the remote cache for the current session"#;

const ASK_RULES: &str = r#"Translate the user's natural language request into exactly one yunpao command.
Reply ONLY with a JSON object: {"explanation": "<one short sentence>", "command": "yunpao ..."}
If the request is unclear or cannot be satisfied with the available commands, set "command" to null and explain in "explanation"."#;

const CHAT_RULES: &str = r#"You are the assistant inside yunpao's local web dashboard. Every user message comes with a SNAPSHOT of the user's real yunpao state: registered servers, sessions, running tasks, actions defined in yunpao.toml, and (when available) live GPU stats.

Rules:
- Answer questions directly from the SNAPSHOT. "How many servers do I have?" → give the number and list them. Never tell the user to run a command to look up something the SNAPSHOT already contains.
- Reply in the language the user writes in. Be concise and friendly; plain text, short lists are fine, no markdown headings.
- When analysing workload, interpret the numbers (e.g. GPU idle while a task is "running" may mean it crashed or is data-loading) instead of just repeating them.
- Choosing a GPU: each GPU line may carry a profile (architecture, whether bf16/flash-attention work, memory, what it suits). Advise from it: memory decides whether a model fits (7B inference ≈ 16G, 7B LoRA ≈ 20G, full fine-tuning ≈ 16 bytes per parameter); bf16/flash-attention need sm_80+; keep top-tier cards (H100 etc.) for jobs that need them when other free cards would do.
- Only when the user asks you to DO something (run, kill, push, pull, view logs, switch session, clear cache, run a shell command remotely…) put exactly one yunpao command in "command". The dashboard shows it with a Copy button; the user pastes it into their terminal. Say what it will do and that they should run it in the terminal; never claim it already happened.
- Prefer commands that finish on their own: use `yunpao logs <action>` rather than `watch`, and never add --watch.
- If the SNAPSHOT lacks the information, say so honestly. Never invent servers, sessions, tasks or numbers.
- If there are no registered servers or no sessions, add one short sentence on how to set them up (`yunpao server new`, then `yunpao session new`), since the user is probably new.
- SUBMITTING WORK: when the user wants to run / train / start something on a server ("用一张空卡跑 train.py，10 个 epoch"), do NOT put it in "command". Fill "job" instead: {"server": "<a server name exactly as listed in the SNAPSHOT>", "command": "<shell command run inside the project folder, e.g. python train.py --epochs 10>", "gpus": <how many GPUs the user wants, 0 if none mentioned and the task doesn't obviously need one, 1 for ordinary training>, "name": "<short task name, letters/digits/-/_ only, usually the script name>"}. Use script names from the SNAPSHOT's project files; never invent a script that isn't listed (if none fits, ask in "reply" and leave "job" null). Pick the server the user named; otherwise the one with free GPUs in the SNAPSHOT; otherwise the default. In "reply" say briefly what you filled in and that they should check it and press 提交. The dashboard picks the actual free GPUs.

- PROGRESS: the SNAPSHOT includes the latest log lines of each task. For "跑到哪了 / loss 在降吗 / 跑完了吗", read them: report the current epoch/step, the trend of loss or accuracy across the lines you can see, and whether it finished (look for "[yunpao] Exit code"). If the log shows no progress lines, say so.
- FORMAT: "reply" may use simple Markdown: short "- " lists, **bold**, `code`. No headings, no tables.

Reply ONLY with a JSON object, "reply" FIRST (it is shown to the user while you write): {"reply": "<your answer>", "command": "yunpao ..." or null, "job": {...} or null}"#;

const DIAGNOSE_RULES: &str = r#"You help a student who knows almost nothing about Linux, SSH or GPUs. Their background job on a remote Linux GPU server has ended. You get the command, the exit code (null = no exit line: the job was killed or stopped before finishing) and the last lines of its log.

Explain in simple Chinese, like a patient senior labmate:
- "summary": one short sentence — what happened.
- "cause": at most 2 short sentences — the most likely reason, quoting the key log line in `backticks`.
- "steps": 1-3 concrete things to do, simplest first, each under 40 Chinese characters. Beginner-friendly (which file to change, what to install, which button to press in the dashboard).
Be brief: the student reads this on a small panel.
- "retry_command": a corrected command to submit again, or null if the fix needs code/data changes first.

Knowledge that matters for this tool:
- Files are uploaded from the user's project folder, but anything listed in .gitignore is NOT uploaded. A FileNotFoundError for a data/model file often means it is git-ignored or lives outside the project folder.
- Packages must be installed on the SERVER, not the laptop: ModuleNotFoundError → `pip install --user <pkg>` on the server, or add it to requirements.txt.
- `CUDA out of memory` → smaller batch size, or pick a GPU that the dashboard shows as free (someone else may be using that card).
- Exit code 137 / "Killed" → the server ran out of RAM (or the job was killed). 130 or 143, or null with no traceback → stopped by the user or the stop button — not a bug.
- Exit 0 means it succeeded; say so plainly.
- If SOURCE is given, point at the exact file and line (e.g. "train.py 第 12 行") and, when the fix is a code change, show the corrected line in `backticks`. The local file may have been edited since upload; if SOURCE doesn't match the traceback, say so.
Only use what the log shows; if it isn't enough to tell, say that and suggest what to look at. Never invent file names or numbers.

Reply ONLY with a JSON object, keys in this order: {"summary": "...", "cause": "...", "steps": ["..."], "retry_command": "..." or null}"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSuggestion {
    pub server: String,
    pub command: String,
    #[serde(default)]
    pub gpus: u32,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatReply {
    pub reply: String,
    pub command: Option<String>,
    pub job: Option<JobSuggestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnosis {
    pub summary: String,
    #[serde(default)]
    pub cause: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub retry_command: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

pub struct AiContext {
    pub servers: Vec<String>,
    pub actions: Vec<String>,
    pub current_session: Option<String>,
}

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// AI settings saved from the GUI settings page (ai.toml next to config.toml).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AiSettings {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeySource {
    File,
    Env,
    None,
}

pub struct ResolvedAi {
    pub api_key: Option<String>,
    pub key_source: KeySource,
    pub base_url: String,
    pub model: String,
}

impl AiSettings {
    pub fn path() -> Result<PathBuf> {
        let mut path = dirs::config_dir().context("Invalid path")?;
        path.push("yunpao");
        path.push("ai.toml");
        Ok(path)
    }

    pub fn load() -> Self {
        Self::path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // The file holds a secret: create it owner-only so it is never readable by others,
        // and tighten an older file that may have been created with looser permissions.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            opts.mode(0o600);
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        use std::io::Write;
        opts.open(&path)?.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }

    /// Settings-page values win over env vars, so saving in the GUI always takes effect.
    /// Key, URL and model are taken as one set from the same source, so a key is
    /// never sent to an endpoint configured for a different provider.
    pub fn resolve(&self) -> ResolvedAi {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let file = |v: &str| Some(v.trim().to_string()).filter(|v| !v.is_empty());

        let (api_key, key_source, base_url, model) = match file(&self.api_key) {
            Some(k) => (Some(k), KeySource::File, file(&self.base_url), file(&self.model)),
            None => {
                let key = env("OPENAI_API_KEY").or_else(|| env("YUNPAO_AI_KEY"));
                let source = if key.is_some() { KeySource::Env } else { KeySource::None };
                (key, source, env("OPENAI_BASE_URL"), env("YUNPAO_AI_MODEL"))
            }
        };
        let base_url = base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());

        ResolvedAi { api_key, key_source, base_url, model }
    }
}

/// Send a tiny request to check that key, URL and model work together.
pub fn test_connection(api_key: &str, base_url: &str, model: &str) -> Result<()> {
    let messages = vec![
        json!({"role": "system", "content": ask_prompt()}),
        json!({"role": "user", "content": "Request: list my servers"}),
    ];
    let raw = call_api(api_key, base_url.trim_end_matches('/'), model, messages)?;
    parse_json(&raw)?;
    Ok(())
}

/// Models this key can use, from the provider's OpenAI-compatible `/models` endpoint.
pub fn list_models(api_key: &str, base_url: &str) -> Result<Vec<String>> {
    let resp = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .context("Failed to reach AI API")?;
    if !resp.status().is_success() {
        bail!("AI API error {}: {}", resp.status(), resp.text().unwrap_or_default());
    }
    let json: Value = resp.json().context("Failed to parse model list")?;
    let mut ids: Vec<String> = json["data"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    ids.sort();
    Ok(ids)
}

fn ask_prompt() -> String {
    format!("You are yunpao's AI assistant. {ABOUT}\n\n{COMMANDS}\n\n{ASK_RULES}")
}

fn credentials() -> Result<(String, String, String)> {
    let ResolvedAi { api_key, base_url, model, .. } = AiSettings::load().resolve();
    let api_key = api_key.context(
        "No AI API key configured. Open `yunpao ui` → ⚙ Settings, or set OPENAI_API_KEY",
    )?;
    Ok((api_key, base_url, model))
}

/// Parse a natural language request into a yunpao command.
/// Returns (explanation, optional_command).
pub fn ask(input: &str, ctx: &AiContext) -> Result<(String, Option<String>)> {
    let (api_key, base_url, model) = credentials()?;

    let context_str = build_context(ctx);
    let user_msg = if context_str.is_empty() {
        input.to_string()
    } else {
        format!("{context_str}\n\nRequest: {input}")
    };

    let messages = vec![
        json!({"role": "system", "content": ask_prompt()}),
        json!({"role": "user", "content": user_msg}),
    ];
    let parsed = parse_json(&call_api(&api_key, &base_url, &model, messages)?)?;

    let explanation = parsed["explanation"]
        .as_str()
        .unwrap_or("(no explanation)")
        .to_string();

    Ok((explanation, command_field(&parsed)))
}

/// Conversational assistant for the dashboard: answers from the state snapshot,
/// and proposes a command only when the user asks for an action.
pub fn chat(
    snapshot: &str,
    history: &[ChatTurn],
    input: &str,
    on_delta: &mut dyn FnMut(&str),
) -> Result<ChatReply> {
    let (api_key, base_url, model) = credentials()?;

    let mut messages = vec![json!({
        "role": "system",
        "content": format!("{CHAT_RULES}\n\n{ABOUT}\n\n{COMMANDS}"),
    })];
    for turn in history.iter().rev().take(12).rev() {
        let role = if turn.role == "user" { "user" } else { "assistant" };
        messages.push(json!({"role": role, "content": turn.content}));
    }
    messages.push(json!({
        "role": "user",
        "content": format!("SNAPSHOT:\n{snapshot}\n\nUSER: {input}"),
    }));

    let parsed = parse_json(&call_api_stream(&api_key, &base_url, &model, messages, on_delta)?)?;
    let reply = parsed["reply"]
        .as_str()
        .or_else(|| parsed["explanation"].as_str())
        .unwrap_or("(empty reply)")
        .to_string();
    let job = serde_json::from_value::<JobSuggestion>(parsed["job"].clone())
        .ok()
        .filter(|j| !j.command.trim().is_empty());
    let command = if job.is_some() { None } else { command_field(&parsed) };
    Ok(ChatReply { reply, command, job })
}

/// Explain a finished (usually failed) job to a beginner from its log tail.
pub fn diagnose(
    command: &str,
    exit_code: Option<i32>,
    log_tail: &str,
    source: &str,
    extra: &str,
    on_delta: &mut dyn FnMut(&str),
) -> Result<Diagnosis> {
    let (api_key, base_url, model) = credentials()?;
    let exit = exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".into());
    let mut user = format!("COMMAND: {command}\nEXIT CODE: {exit}\n");
    if !extra.trim().is_empty() {
        user.push_str(&format!("DASHBOARD CONTEXT:\n{extra}\n"));
    }
    user.push_str(&format!("LAST LOG LINES:\n{log_tail}\n"));
    if !source.trim().is_empty() {
        user.push_str(&format!("\nSOURCE around the traceback lines (from the user's local project folder; `>>` marks the reported line):\n{source}"));
    }
    let messages = vec![
        json!({"role": "system", "content": DIAGNOSE_RULES}),
        json!({"role": "user", "content": user}),
    ];
    let parsed = parse_json(&call_api_stream(&api_key, &base_url, &model, messages, on_delta)?)?;
    serde_json::from_value(parsed).context("AI returned an unexpected diagnosis format")
}

fn parse_json(raw: &str) -> Result<Value> {
    // Some models wrap JSON in ```json fences despite response_format.
    let t = raw.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    serde_json::from_str(t).context(format!("AI returned invalid JSON: {raw}"))
}

fn command_field(v: &Value) -> Option<String> {
    v["command"]
        .as_str()
        .map(str::trim)
        .filter(|s| s.starts_with("yunpao "))
        .map(String::from)
}

fn build_context(ctx: &AiContext) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !ctx.servers.is_empty() {
        parts.push(format!("Registered servers: {}", ctx.servers.join(", ")));
    }
    if !ctx.actions.is_empty() {
        parts.push(format!("Actions in yunpao.toml: {}", ctx.actions.join(", ")));
    }
    if let Some(session) = &ctx.current_session {
        parts.push(format!("Active session: {session}"));
    }
    parts.join("\n")
}

fn call_api(api_key: &str, base_url: &str, model: &str, messages: Vec<Value>) -> Result<String> {
    // The SJTU endpoint sometimes takes 30-60 s under load; don't give up too early.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("Failed to build HTTP client")?;

    let body = json!({
        "model": model,
        "messages": messages,
        "temperature": 0.1,
        "response_format": {"type": "json_object"}
    });

    let resp = client
        .post(format!("{base_url}/chat/completions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("Failed to reach AI API — check your internet connection")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("AI API error {status}: {text}");
    }

    let json: Value = resp.json().context("Failed to parse API response")?;

    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .context("Unexpected API response format")?
        .to_string();

    Ok(content)
}

/// Streaming variant (OpenAI-compatible SSE): `on_delta` receives text as it arrives;
/// returns the whole text.
fn call_api_stream(
    api_key: &str,
    base_url: &str,
    model: &str,
    messages: Vec<Value>,
    on_delta: &mut dyn FnMut(&str),
) -> Result<String> {
    use std::io::BufRead;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("Failed to build HTTP client")?;
    let body = json!({
        "model": model,
        "messages": messages,
        "temperature": 0.1,
        "response_format": {"type": "json_object"},
        "stream": true
    });
    let resp = client
        .post(format!("{base_url}/chat/completions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("Failed to reach AI API — check your internet connection")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("AI API error {status}: {text}");
    }
    let mut full = String::new();
    for line in std::io::BufReader::new(resp).lines() {
        let line = line.context("AI stream interrupted")?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(delta) = v["choices"][0]["delta"]["content"].as_str()
            && !delta.is_empty()
        {
            full.push_str(delta);
            on_delta(delta);
        }
    }
    ensure!(!full.is_empty(), "AI returned an empty response");
    Ok(full)
}
