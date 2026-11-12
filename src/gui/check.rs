//! "Pre-flight" check of a training script before submitting it. Pure rules plus one ssh call,
//! no AI: catches the mistakes that most often waste a beginner's run.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::io::{AsyncBufReadExt, BufReader};

use super::{AppState, ev, jobs::{safe_env_path, safe_rel}, sse_from, ssh_base_args, ssh_exec};

#[derive(Deserialize)]
pub struct CheckReq {
    server_id: String,
    command: String,
    #[serde(default)]
    python_env: Option<String>,
    /// How many GPUs are picked in the form.
    #[serde(default)]
    gpus: usize,
    /// The picked GPUs (model name and memory), for compatibility and memory checks.
    #[serde(default)]
    selected: Vec<SelGpu>,
}

#[derive(Deserialize)]
pub struct SelGpu {
    index: u32,
    name: String,
    mem_total: u32,
}

#[derive(Serialize)]
pub struct Check {
    /// "ok" | "warn" | "bad" | "info"
    level: &'static str,
    title: String,
    detail: String,
    /// pip packages that would fix this; the page offers a one-click install.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    install: Vec<String>,
}

#[derive(Serialize, Default)]
pub struct CheckResp {
    ok: bool,
    script: String,
    checks: Vec<Check>,
}

fn check(level: &'static str, title: impl Into<String>, detail: impl Into<String>) -> Check {
    Check { level, title: title.into(), detail: detail.into(), install: Vec::new() }
}

const ML_LIBS: &[&str] = &["torch", "tensorflow", "keras", "jax", "transformers", "lightning", "pytorch_lightning", "accelerate"];
const GPU_HINTS: &[&str] = &[".cuda(", "\"cuda", "'cuda", "torch.device(", "device_map", "accelerate", "Trainer(", "jax.devices", "tf.config", ".to(device", "to(self.device"];
const SAVE_HINTS: &[&str] = &[
    "torch.save", "save_pretrained", "save_model", "savefig", "to_csv", "np.save", "json.dump", "pickle.dump",
    "joblib.dump", "ModelCheckpoint", ".save(", "save_checkpoint", "SummaryWriter", "wandb.log", "imwrite", "imsave",
    "sf.write", "soundfile.write", "wavfile.write", "torchaudio.save", "write_videofile", ".to_parquet", "\"ffmpeg\"",
];
const READ_HINTS: &[&str] = &["open(", "read_csv", "read_json", "read_parquet", "np.load", "torch.load", "imread", "Image.open", "load_dataset", "loadtxt", "ImageFolder", "default="];
const DATA_EXTS: &[&str] = &[".csv", ".tsv", ".json", ".jsonl", ".txt", ".npy", ".npz", ".pt", ".pth", ".ckpt", ".pkl", ".h5", ".hdf5", ".parquet", ".jpg", ".jpeg", ".png", ".zip", ".tar", ".gz", ".bin", ".safetensors", ".yaml", ".yml"];

/// "Qwen/Qwen2.5-7B-Instruct" → 7.0 (billions of parameters).
fn model_params(lit: &str) -> Option<f64> {
    if !lit.contains(['/', '-']) || lit.len() > 120 {
        return None;
    }
    lit.to_ascii_lowercase()
        .split(['-', '_', '/', ' ', ':'])
        .filter_map(|seg| seg.strip_suffix('b'))
        .filter(|n| n.chars().next().is_some_and(|c| c.is_ascii_digit()) && n.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .filter_map(|n| n.parse::<f64>().ok())
        .find(|&b| b > 0.0 && b <= 1000.0)
}

/// Rough GPU memory (GiB) for a model of `params` billions, from how the code uses it.
fn estimate_gib(params: f64, code: &str) -> (f64, &'static str) {
    let q4 = code.contains("load_in_4bit") || code.contains("bnb_4bit");
    let q8 = code.contains("load_in_8bit");
    let weights = if q4 { 0.5 } else if q8 { 1.0 } else { 2.0 }; // bytes per parameter
    let lora = code.contains("LoraConfig") || code.contains("get_peft_model");
    let train = code.contains(".backward(") || code.contains("Trainer(") || code.contains("optimizer");
    let (bytes, how) = if lora {
        (weights * 1.3, if q4 { "QLoRA 微调" } else { "LoRA 微调" })
    } else if train {
        (16.0, "全量训练") // fp16 weights + grads + Adam states in fp32
    } else {
        (weights, if q4 { "4-bit 推理" } else if q8 { "8-bit 推理" } else { "推理" })
    };
    (params * bytes * 1.2 + 1.0, how)
}

/// bf16 / flash-attention vs. old cards, and whether a recognised model fits.
fn gpu_fit_checks(all: &str, sources: &[(String, String)], selected: &[SelGpu], checks: &mut Vec<Check>) {
    const NEEDS_AMPERE: &[(&str, &str)] = &[
        ("bf16=True", "bf16"), ("--bf16", "bf16"), ("bfloat16", "bf16"),
        ("flash_attn", "flash-attention"), ("flash_attention_2", "flash-attention"),
    ];
    let mut feats: Vec<&str> = NEEDS_AMPERE.iter().filter(|(k, _)| all.contains(k)).map(|(_, f)| *f).collect();
    feats.dedup();
    if !feats.is_empty() {
        let old: Vec<String> = selected
            .iter()
            .filter_map(|g| super::gpu_db::profile(&g.name, g.mem_total).filter(|p| !p.bf16).map(|p| format!("GPU {}（{}）", g.index, p.arch)))
            .collect();
        if !old.is_empty() {
            checks.push(check("bad", format!("{} 不支持代码里用到的 {}", old.join("、"), feats.join(" / ")),
                "这些功能要 Ampere（RTX 30 系、A100）或更新的卡。换一张新一点的卡，或者把 bf16 改成 fp16、去掉 flash-attention。"));
        }
    }

    // The biggest model named in a string literal, e.g. from_pretrained("Qwen/Qwen2.5-7B").
    let model = sources
        .iter()
        .flat_map(|(_, src)| src.lines().filter(|l| !l.trim_start().starts_with('#')).flat_map(literals))
        .filter_map(|lit| model_params(&lit).map(|b| (lit, b)))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    let Some((name, params)) = model else { return };
    let (need, how) = estimate_gib(params, all);
    let need_s = format!("{need:.0}G");
    let have: u32 = selected.iter().map(|g| (g.mem_total + 512) / 1024).sum();
    let what = format!("{name}（约 {params}B 参数，{how}）");
    let note = "粗略估计，batch size 和序列长度大了会更多。";
    if selected.is_empty() {
        checks.push(check("info", format!("{what}大约要 {need_s} 显存"), format!("选一张显存不少于 {need_s} 的卡。{note}")));
    } else if (have as f64) < need {
        let tip = if how == "全量训练" { "可以改用 LoRA（peft），显存能省很多；" } else if !how.contains("4-bit") && !how.contains("QLoRA") { "可以用 4-bit 量化（load_in_4bit）；" } else { "" };
        checks.push(check("warn", format!("显存可能不够：{what}大约要 {need_s}，你选的卡一共 {have}G"),
            format!("多选几张卡（配合 device_map=\"auto\"），{tip}或者换显存更大的卡。{note}")));
    } else {
        checks.push(check("ok", format!("显存够用：{what}大约要 {need_s}，你选的卡有 {have}G"), note));
    }
}

/// Import name → pip package, only where we're sure. Installing by import name alone goes
/// wrong too often (`whisper`, `clip`, `fitz` are other projects on PyPI), so anything not
/// listed gets no one-click install.
fn pip_name(module: &str) -> Option<&'static str> {
    const RENAMED: &[(&str, &str)] = &[
        ("cv2", "opencv-python"), ("PIL", "pillow"), ("sklearn", "scikit-learn"), ("yaml", "pyyaml"),
        ("skimage", "scikit-image"), ("bs4", "beautifulsoup4"), ("pytorch_lightning", "pytorch-lightning"),
        ("whisper", "openai-whisper"), ("fitz", "PyMuPDF"), ("dotenv", "python-dotenv"), ("jwt", "PyJWT"),
        ("docx", "python-docx"), ("pptx", "python-pptx"), ("hydra", "hydra-core"), ("Crypto", "pycryptodome"),
        ("serial", "pyserial"), ("magic", "python-magic"), ("lightning", "lightning"), ("faiss", "faiss-cpu"),
        ("google.protobuf", "protobuf"), ("speech_recognition", "SpeechRecognition"), ("pydub", "pydub"),
    ];
    const SAME: &[&str] = &[
        "numpy", "pandas", "scipy", "matplotlib", "seaborn", "torch", "torchvision", "torchaudio", "transformers",
        "datasets", "accelerate", "peft", "trl", "diffusers", "evaluate", "tokenizers", "sentencepiece", "safetensors",
        "bitsandbytes", "einops", "timm", "tqdm", "requests", "librosa", "soundfile", "tensorboard", "wandb", "jieba",
        "nltk", "onnx", "onnxruntime", "numba", "h5py", "lightgbm", "xgboost", "openai", "gradio", "rich", "pydantic",
        "fastapi", "flask", "omegaconf", "tensorflow", "keras", "jsonlines", "pyarrow", "plotly", "networkx",
        "sympy", "statsmodels", "joblib", "pyworld", "praat-parselmouth", "tiktoken", "langchain", "spacy", "gensim",
        "albumentations", "imageio", "openpyxl", "xlrd", "psutil", "click", "typer", "loguru", "pytest", "ujson", "orjson",
    ];
    RENAMED.iter().find(|(m, _)| *m == module).map(|(_, p)| *p).or_else(|| SAME.iter().find(|p| **p == module).copied())
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// Top-level module names imported by a Python source (absolute imports only).
fn imports(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in src.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("import ") {
            for part in rest.split(',') {
                let name = part.trim().split_whitespace().next().unwrap_or("");
                let top = name.split('.').next().unwrap_or("");
                if is_ident(top) {
                    out.insert(top.to_string());
                }
            }
        } else if let Some(rest) = l.strip_prefix("from ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            if !name.starts_with('.') {
                let top = name.split('.').next().unwrap_or("");
                if is_ident(top) {
                    out.insert(top.to_string());
                }
            }
        }
    }
    out
}

/// The script plus project modules it imports directly (one level), with external imports.
fn gather(dir: &Path, script: &str) -> (Vec<(String, String)>, BTreeSet<String>) {
    let read = |p: &Path| std::fs::read(p).ok().map(|b| String::from_utf8_lossy(&b[..b.len().min(400_000)]).into_owned());
    let mut sources = Vec::new();
    let Some(main) = read(&dir.join(script)) else { return (sources, BTreeSet::new()) };
    let script_dir = Path::new(script).parent().unwrap_or(Path::new(""));
    let mut external = BTreeSet::new();
    let mut local_files: Vec<PathBuf> = Vec::new();
    for m in imports(&main) {
        // Python looks next to the script first.
        // …and the project root (the job runs from there), so `tools/train.py` importing the
        // root's `utils` isn't mistaken for a PyPI package.
        let base = [dir.join(script_dir), dir.to_path_buf()].into_iter().find(|b| b.join(format!("{m}.py")).is_file() || b.join(&m).is_dir());
        if let Some(base) = base {
            if base.join(format!("{m}.py")).is_file() {
                local_files.push(base.join(format!("{m}.py")));
            } else if let Ok(entries) = std::fs::read_dir(base.join(&m)) {
                local_files.extend(entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "py")));
            }
        } else {
            external.insert(m);
        }
    }
    sources.push((script.to_string(), main));
    for f in local_files.into_iter().take(20) {
        if let Some(text) = read(&f) {
            for m in imports(&text) {
                let local = |b: PathBuf| b.join(format!("{m}.py")).is_file() || b.join(&m).is_dir();
                if !local(dir.join(script_dir)) && !local(dir.to_path_buf()) {
                    external.insert(m);
                }
            }
            sources.push((f.strip_prefix(dir).unwrap_or(&f).display().to_string(), text));
        }
    }
    (sources, external)
}

/// Quoted string literals on a line.
fn literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '"' || c == '\'' {
            let start = i + 1;
            let mut end = None;
            while let Some((j, d)) = chars.next() {
                if d == '\\' {
                    chars.next();
                } else if d == c {
                    end = Some(j);
                    break;
                }
            }
            if let Some(e) = end {
                out.push(line[start..e].to_string());
            }
        } else if c == '#' {
            break;
        }
    }
    out
}

/// Files the upload would send from this folder (local dry run with the same excludes).
fn upload_set(dir: &Path) -> HashSet<String> {
    let mut cmd = std::process::Command::new("rsync");
    cmd.arg("-azn").arg("--out-format=%n");
    if dir.join(".git").exists() {
        cmd.args(["-f", "- /.git/"]);
    }
    if dir.join(".gitignore").exists() {
        cmd.arg(format!("--exclude-from={}", dir.join(".gitignore").display()));
    }
    cmd.arg(format!("{}/", dir.display()));
    cmd.arg(std::env::temp_dir().join(format!("yunpao-check-{}", crate::utils::random_uuid())));
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().trim_end_matches('/').to_string()).collect())
        .unwrap_or_default()
}

fn rule_checks(dir: &Path, sources: &[(String, String)], external: &BTreeSet<String>, gpus: usize, checks: &mut Vec<Check>) {
    let all: String = sources.iter().map(|s| s.1.as_str()).collect::<Vec<_>>().join("\n");
    let uses_ml = external.iter().any(|m| ML_LIBS.contains(&m.as_str()));

    // GPU
    if GPU_HINTS.iter().any(|h| all.contains(h)) {
        checks.push(check("ok", "代码会用到 GPU", ""));
    } else if uses_ml {
        checks.push(check("warn", "没找到把模型放到 GPU 上的代码",
            "没看到 .cuda() / .to(device) 之类的写法，可能只在 CPU 上跑：很慢，还白占一张卡。"));
    } else if gpus > 0 {
        checks.push(check("warn", "这个脚本用不到显卡",
            "没有用到 torch / tensorflow 等框架，在 CPU 上就能跑。把下面选中的卡取消掉，留给需要的同学。"));
    } else {
        checks.push(check("info", "看起来不是深度学习训练脚本", "没有用到 torch / tensorflow 等框架，不需要选显卡。"));
    }

    // Hard-coded GPU choice overrides the one picked in the form.
    let hard = all.lines().find(|l| {
        let t = l.trim_start();
        !t.starts_with('#') && (t.contains("CUDA_VISIBLE_DEVICES") || t.contains("cuda:1") || t.contains("cuda:2") || t.contains("cuda:3") || t.contains("set_device("))
    });
    if let Some(l) = hard {
        checks.push(check("warn", "代码里写死了显卡",
            format!("`{}` 会覆盖你在表单里选的卡。表单里选好卡之后，代码里用 \"cuda\" 或 \"cuda:0\" 就行。", l.trim().chars().take(80).collect::<String>())));
    }

    // Results saved?
    if SAVE_HINTS.iter().any(|h| all.contains(h)) {
        checks.push(check("ok", "会把结果保存到文件", "跑完可以用「⬇ 取回结果」下载。"));
    } else {
        checks.push(check("warn", "没找到保存结果的代码",
            "没看到 torch.save / savefig / to_csv 之类的写法。跑完只能在日志里看输出，模型和图都拿不回来。"));
    }

    // Data files referenced by the code.
    let uploaded = upload_set(dir);
    let mut seen = HashSet::new();
    for (file, src) in sources {
        for line in src.lines().filter(|l| !l.trim_start().starts_with('#')) {
            let reading = READ_HINTS.iter().any(|h| line.contains(h)) && !line.contains("'w") && !line.contains("\"w");
            for lit in literals(line) {
                let looks_like_path = (lit.contains('/') || DATA_EXTS.iter().any(|e| lit.to_lowercase().ends_with(e)))
                    && !lit.starts_with("http") && !lit.starts_with("--") && !lit.contains('{') && !lit.contains('%')
                    && !lit.contains(' ') && lit.len() < 200 && !lit.starts_with("gs://") && !lit.starts_with("s3://");
                if !looks_like_path || !seen.insert(lit.clone()) {
                    continue;
                }
                if lit.starts_with('/') || lit.starts_with('~') {
                    if reading {
                        checks.push(check("info", format!("用到服务器上的绝对路径 {lit}"),
                            format!("在 {file} 里。这个路径要在服务器上存在，不会从你电脑上传。")));
                    }
                    continue;
                }
                let rel = lit.trim_start_matches("./").trim_end_matches('/');
                if !safe_rel(rel) {
                    continue;
                }
                let local = dir.join(Path::new(file).parent().unwrap_or(Path::new(""))).join(rel);
                let local = if local.exists() { local } else { dir.join(rel) };
                let rel_to_dir = local.strip_prefix(dir).map(|p| p.display().to_string()).unwrap_or_else(|_| rel.to_string());
                if local.exists() {
                    let sent = uploaded.contains(&rel_to_dir) || uploaded.iter().any(|u| u.starts_with(&format!("{rel_to_dir}/")));
                    if sent {
                        checks.push(check("ok", format!("数据 {rel_to_dir} 会一起上传"), ""));
                    } else {
                        checks.push(check("bad", format!("数据 {rel_to_dir} 不会被上传"),
                            "它被 .gitignore 排除了，服务器上会找不到，任务一启动就会报 FileNotFoundError。把它从 .gitignore 里去掉，或者事先把数据放到服务器上并改用服务器路径。"));
                    }
                } else if reading {
                    checks.push(check("warn", format!("项目里找不到 {rel}"),
                        format!("{file} 会读取它，但你电脑上的项目文件夹里没有。如果它只在服务器上，没问题；否则任务会报 FileNotFoundError。")));
                }
            }
        }
    }

    // Tunable parameters.
    let mut params = Vec::new();
    for line in all.lines().filter(|l| l.contains("add_argument(")) {
        let lits = literals(line);
        if let Some(flag) = lits.iter().find(|l| l.starts_with("--")) {
            let default = line.split("default=").nth(1).map(|d| d.split([',', ')']).next().unwrap_or("").trim().trim_matches(['"', '\'']).to_string());
            params.push(match default {
                Some(d) if !d.is_empty() => format!("{flag}（默认 {d}）"),
                _ => flag.clone(),
            });
        }
    }
    if !params.is_empty() {
        let n = params.len();
        params.truncate(8);
        checks.push(check("info", format!("可以调的参数：{n} 个"),
            format!("{}{}。直接加在命令后面，例如 python {} {} …", params.join("、"), if n > 8 { " 等" } else { "" },
                sources.first().map(|s| s.0.as_str()).unwrap_or("train.py"), params[0].split('（').next().unwrap_or(""))));
    }
}

pub async fn api_check(State(app): State<Arc<AppState>>, Json(mut req): Json<CheckReq>) -> Json<CheckResp> {
    let words = shlex::split(&req.command).unwrap_or_default();
    let Some(script) = words.iter().find(|w| w.ends_with(".py")).map(|w| w.trim_start_matches("./").to_string()) else {
        let msg = if words.iter().any(|w| w.ends_with(".sh")) { "这是 shell 脚本，体检只支持 Python 脚本。" } else { "命令里没有 .py 文件，跳过体检。" };
        return Json(CheckResp { ok: true, checks: vec![check("info", msg, "")], ..Default::default() });
    };
    if !safe_rel(&script) || !app.project_dir.join(&script).is_file() {
        return Json(CheckResp { ok: true, checks: vec![check("bad", format!("项目文件夹里找不到 {script}"), "检查文件名，或者确认 yunpao ui 是在项目文件夹里启动的。")], script, ..Default::default() });
    }

    let dir = app.project_dir.clone();
    let s = script.clone();
    let gpus = req.gpus.max(req.selected.len());
    let selected = std::mem::take(&mut req.selected);
    let (mut checks, external) = tokio::task::spawn_blocking(move || {
        let (sources, external) = gather(&dir, &s);
        let mut checks = Vec::new();
        rule_checks(&dir, &sources, &external, gpus, &mut checks);
        let all: String = sources.iter().map(|s| s.1.as_str()).collect::<Vec<_>>().join("\n");
        gpu_fit_checks(&all, &sources, &selected, &mut checks);
        (checks, external)
    })
    .await
    .unwrap_or_default();

    // Dependencies, in the Python environment the job will actually use.
    let config = app.global();
    let env_ok = req.python_env.as_deref().is_none_or(safe_env_path);
    if let (Some(server), true) = (config.servers.get(&req.server_id), env_ok) {
        let names: Vec<&str> = external.iter().map(String::as_str).filter(|m| is_ident(m)).take(40).collect();
        let py = req.python_env.as_deref().map(|e| format!("{e}/bin/python")).unwrap_or_else(|| "python3".into());
        let probe = r#"import importlib.util as u, sys
names = sys.argv[1:]
std = set(getattr(sys, "stdlib_module_names", ())) | set(sys.builtin_module_names)
miss = [n for n in names if n not in std and u.find_spec(n) is None]
print("MISSING " + " ".join(miss))
if "torch" in names and "torch" not in miss:
    import torch
    print("TORCH %s %s %d" % (torch.__version__, torch.cuda.is_available(), torch.cuda.device_count()))"#;
        let cmd = format!("{py} -c '{probe}' {} 2>&1 | tail -5", names.join(" "));
        match ssh_exec(server, &cmd).await {
            Ok((_, out)) => {
                if let Some(miss) = out.lines().find_map(|l| l.strip_prefix("MISSING")) {
                    let miss: Vec<&str> = miss.split_whitespace().collect();
                    if miss.is_empty() {
                        checks.push(check("ok", "用到的包在这个环境里都装了", ""));
                    } else {
                        let known: Vec<String> = miss.iter().filter_map(|m| pip_name(m)).map(String::from).collect();
                        let unsure: Vec<&str> = miss.iter().copied().filter(|m| pip_name(m).is_none()).collect();
                        let mut detail = String::from("换一个装过这些包的 Python 环境");
                        if !known.is_empty() {
                            detail.push_str("，或者点下面的按钮装到当前环境里");
                        }
                        detail.push('。');
                        if !unsure.is_empty() {
                            detail.push_str(&format!(" {} 不确定对应 pip 上的哪个包（同名的可能是别的项目），请看这个项目的 README / requirements.txt 确认后，在「⌨ 终端」里自己装。", unsure.join("、")));
                        }
                        if known.iter().any(|p| p == "torch" || p == "torchvision" || p == "torchaudio") {
                            detail.push_str(" torch 会装 PyPI 默认的 CUDA 版本，装完体检会检查它能不能用 GPU。");
                        }
                        let mut c = check("bad", format!("这个环境里缺少：{}", miss.join("、")), detail);
                        c.install = known;
                        checks.push(c);
                    }
                } else {
                    checks.push(check("warn", "没能检查依赖", out.lines().last().unwrap_or("").to_string()));
                }
                if let Some(t) = out.lines().find_map(|l| l.strip_prefix("TORCH ")) {
                    let p: Vec<&str> = t.split_whitespace().collect();
                    if p.get(1) == Some(&"True") {
                        checks.push(check("ok", format!("PyTorch {} 能用 GPU（{} 张卡可见）", p[0], p.get(2).unwrap_or(&"?")), ""));
                    } else {
                        checks.push(check("bad", format!("这个环境里的 PyTorch {} 用不了 GPU", p.first().unwrap_or(&"")),
                            "装的可能是 CPU 版 torch，或者 CUDA 版本和驱动不匹配。换一个环境，或者按 pytorch.org 的说明重装 GPU 版。"));
                    }
                }
            }
            Err(e) => checks.push(check("warn", "连不上服务器，没检查依赖", e)),
        }
    }

    // Problems first.
    let rank = |l: &str| match l { "bad" => 0, "warn" => 1, "ok" => 2, _ => 3 };
    checks.sort_by_key(|c| rank(c.level));
    Json(CheckResp { ok: true, script, checks })
}

// ── Runtime: is a running job actually on the GPU? ──────────────────────────

#[derive(Serialize)]
pub struct GpuUse {
    gpu: u32,
    mem_mib: u32,
    /// SM utilisation %; None when nvidia-smi shows "-".
    sm: Option<u32>,
}

#[derive(Serialize)]
pub struct TaskGpu {
    session_id: String,
    name: String,
    elapsed_secs: u64,
    /// The job was given GPUs, or its script uses a deep-learning framework.
    expects_gpu: bool,
    on_gpu: Vec<GpuUse>,
    progress: Option<Progress>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Progress {
    done: u64,
    total: u64,
    /// "epoch" (from `Epoch 3/10`-style prints) or "step" (a tqdm bar).
    unit: &'static str,
    eta_secs: Option<u64>,
}

/// "01:23" / "1:02:03" → seconds.
fn hms(s: &str) -> Option<u64> {
    s.trim().split(':').try_fold(0u64, |acc, p| p.parse::<u64>().ok().map(|n| acc * 60 + n))
}

/// `… 45%|████| 450/1000 [01:23<01:40, 5.4it/s]` → (450, 1000, remaining secs).
fn tqdm_line(line: &str) -> Option<(u64, u64, Option<u64>)> {
    let b = line.rfind('[')?;
    let (elapsed, rest) = line[b + 1..].split_once('<')?;
    hms(elapsed)?;
    let remaining = hms(rest.split([',', ']']).next()?);
    let (d, t) = line[..b].split_whitespace().last()?.split_once('/')?;
    let (done, total) = (d.parse().ok()?, t.parse().ok()?);
    (total > 0).then_some((done, total, remaining))
}

/// `Epoch 3/10`, `epoch: [3/10]`, `Epoch 3 of 10` → (3, 10).
fn epoch_line(line: &str) -> Option<(u64, u64)> {
    let low = line.to_ascii_lowercase();
    let at = low.rfind("epoch")?;
    let rest = low[at + 5..].trim_start_matches(|c: char| c == ' ' || c == ':' || c == '[' || c == '=');
    let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let rest = rest[n.len()..].trim_start();
    let rest = rest.strip_prefix('/').or_else(|| rest.strip_prefix("of"))?.trim_start();
    let t: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let (done, total) = (n.parse().ok()?, t.parse().ok()?);
    (total > 0 && done <= total).then_some((done, total))
}

/// Latest progress in a log tail. Epochs beat a tqdm bar (often per-epoch); the ETA from
/// epochs is a straight-line guess from how long the job has run.
fn parse_progress(log: &str, elapsed: u64) -> Option<Progress> {
    let lines: Vec<&str> = log.lines().collect();
    if let Some((done, total)) = lines.iter().rev().find_map(|l| epoch_line(l)) {
        let finished = done.saturating_sub(1).max(1);
        let eta_secs = (done > 1 && elapsed > 0).then(|| elapsed * (total - finished) / finished);
        return Some(Progress { done, total, unit: "epoch", eta_secs });
    }
    let (done, total, eta_secs) = lines.iter().rev().find_map(|l| tqdm_line(l))?;
    Some(Progress { done, total, unit: "step", eta_secs })
}

/// Per-pid GPU use from `nvidia-smi pmon -s um`, located by header so driver versions with
/// different columns still parse.
fn parse_pmon(out: &str) -> Vec<(u32, u32, u32, Option<u32>)> {
    let mut cols: Option<(usize, usize)> = None;
    let mut rows = Vec::new();
    for line in out.lines() {
        if let Some(h) = line.strip_prefix('#') {
            let names: Vec<&str> = h.split_whitespace().collect();
            if names.first() == Some(&"gpu")
                && let (Some(sm), Some(fb)) = (names.iter().position(|c| *c == "sm"), names.iter().position(|c| *c == "fb"))
            {
                cols = Some((sm, fb));
            }
            continue;
        }
        let Some((sm, fb)) = cols else { continue };
        let f: Vec<&str> = line.split_whitespace().collect();
        let (Some(gpu), Some(pid)) = (f.first().and_then(|v| v.parse().ok()), f.get(1).and_then(|v| v.parse().ok())) else { continue };
        let mem = f.get(fb).and_then(|v| v.parse().ok()).unwrap_or(0);
        rows.push((pid, gpu, mem, f.get(sm).and_then(|v| v.parse().ok())));
    }
    rows
}

/// Whether the job is meant to use a GPU, judged from its remote command line.
fn expects_gpu(dir: &Path, args: &str) -> bool {
    if args.contains("CUDA_VISIBLE_DEVICES") {
        return true;
    }
    args.split_whitespace()
        .map(|w| w.trim_matches(['\'', '"', ';']).trim_start_matches("./"))
        .find(|w| w.ends_with(".py") && safe_rel(w))
        .is_some_and(|script| gather(dir, script).1.iter().any(|m| ML_LIBS.contains(&m.as_str())))
}

const PMON_MARK: &str = "===YUNPAO_PMON===";

/// GPU use and training progress of every running job (one ssh call per server).
pub async fn api_gpu_usage(State(app): State<Arc<AppState>>) -> Json<Vec<TaskGpu>> {
    let config = app.global();
    let state = crate::storage::ProjectState::load(&app.project_id).unwrap_or_default();
    let mut by_server: std::collections::HashMap<String, Vec<(String, String, String)>> = Default::default();
    for s in state.list_sessions() {
        for (name, pid) in &s.tasks {
            if super::is_safe_token(name) && !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) {
                by_server.entry(s.server.clone()).or_default().push((s.id.clone(), name.clone(), pid.clone()));
            }
        }
    }

    let mut out = Vec::new();
    for (server_id, tasks) in by_server {
        let Some(server) = config.servers.get(&server_id) else { continue };
        // Jobs are started with setsid, so everything they spawn shares session id = pid.
        let mut script: String = tasks
            .iter()
            .map(|(sid, name, pid)| {
                format!(
                    "if {alive}; then echo \"T {sid} {name} $(ps -o etimes= -p {pid} | tr -d ' ') $( (echo {pid}; ps --sid {pid} -o pid= 2>/dev/null) | tr -s ' \\n' ',')\"; \
                     echo \"A {sid} {name} $(ps -o args= -p {pid} | head -c 3000 | tr '\\n' ' ')\"; \
                     echo 'L {sid} {name}'; tail -c 8000 {log} 2>/dev/null | tr '\\r' '\\n' | grep -v '^ *$' | tail -40 | sed 's/^/| /'; fi;",
                    alive = crate::ssh::task_alive_check(pid),
                    log = super::remote_log_path(server, sid, name)
                )
            })
            .collect();
        script.push_str(&format!("echo '{PMON_MARK}'; nvidia-smi pmon -c 1 -s um 2>/dev/null"));
        let Ok((_, text)) = ssh_exec(server, &script).await else { continue };
        let (head, pmon) = text.split_once(PMON_MARK).unwrap_or((&text, ""));
        let usage = parse_pmon(pmon);

        let mut args: std::collections::HashMap<(String, String), String> = Default::default();
        let mut logs: std::collections::HashMap<(String, String), String> = Default::default();
        let mut found = Vec::new();
        let mut cur_log: Option<(String, String)> = None;
        for line in head.lines() {
            if let Some(l) = line.strip_prefix("| ") {
                if let Some(k) = &cur_log {
                    let buf = logs.entry(k.clone()).or_default();
                    buf.push_str(l);
                    buf.push('\n');
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("L ")
                && let Some((sid, name)) = rest.split_once(' ')
            {
                cur_log = Some((sid.into(), name.into()));
                continue;
            }
            let mut p = line.splitn(4, ' ');
            match (p.next(), p.next(), p.next(), p.next()) {
                (Some("T"), Some(sid), Some(name), Some(rest)) => {
                    let mut r = rest.split_whitespace();
                    let elapsed = r.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let pids: HashSet<u32> = r.next().unwrap_or("").split(',').filter_map(|v| v.parse().ok()).collect();
                    found.push((sid.to_string(), name.to_string(), elapsed, pids));
                }
                (Some("A"), Some(sid), Some(name), rest) => {
                    args.insert((sid.into(), name.into()), rest.unwrap_or("").to_string());
                }
                _ => {}
            }
        }
        for (sid, name, elapsed_secs, pids) in found {
            let a = args.remove(&(sid.clone(), name.clone())).unwrap_or_default();
            let progress = logs.get(&(sid.clone(), name.clone())).and_then(|l| parse_progress(l, elapsed_secs));
            let dir = app.project_dir.clone();
            let expects_gpu = tokio::task::spawn_blocking(move || expects_gpu(&dir, &a)).await.unwrap_or(false);
            let on_gpu = usage
                .iter()
                .filter(|u| pids.contains(&u.0))
                .map(|&(_, gpu, mem_mib, sm)| GpuUse { gpu, mem_mib, sm })
                .collect();
            out.push(TaskGpu { session_id: sid, name, elapsed_secs, expects_gpu, on_gpu, progress });
        }
    }
    Json(out)
}

#[derive(Deserialize)]
pub struct InstallReq {
    server_id: String,
    #[serde(default)]
    python_env: Option<String>,
    packages: Vec<String>,
}

fn is_pkg_name(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

const PIP_MIRROR: &str = "https://mirror.sjtu.edu.cn/pypi/web/simple";
const RC_MARK: &str = "__YUNPAO_RC=";

/// `pip install` into the job's environment. Runs detached on the server (closing the page
/// doesn't abort a 2 GB torch download) and streams its log.
/// Events: `line` {text}, then `done` {ok, code} or `error` {message}.
pub async fn api_install(State(app): State<Arc<AppState>>, Json(req): Json<InstallReq>) -> super::SseStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let fail = |m: &str| { let _ = tx.send(ev("error", serde_json::json!({ "message": m }))); };
        let config = app.global();
        let Some(server) = config.servers.get(&req.server_id) else { return fail("找不到这台服务器") };
        if req.packages.is_empty() || req.packages.len() > 40 || !req.packages.iter().all(|p| is_pkg_name(p)) {
            return fail("包名不合法");
        }
        if !req.python_env.as_deref().is_none_or(safe_env_path) {
            return fail("Python 环境路径不合法");
        }
        let py = req.python_env.as_deref().map(|e| format!("{e}/bin/python")).unwrap_or_else(|| "python3".into());
        // Use --user only when the environment itself isn't writable (system Python).
        // Respect the user's own pip index if they configured one; else the campus mirror, then PyPI.
        let inner = format!(
            r#"PY="$1"; shift
if [ -w "$("$PY" -c "import sys;print(sys.prefix)")" ]; then U=; else U=--user; fi
P="$PY -m pip install $U --progress-bar off"
if "$PY" -m pip config list 2>/dev/null | grep -q index-url; then $P "$@"
else $P -i {PIP_MIRROR} "$@" || {{ echo "镜像源失败，改用官方源重试…"; $P "$@"; }}; fi
echo "{RC_MARK}$?""#
        );
        let pkgs = req.packages.join(" ");
        let cmd = format!(
            // `;` not `&&`: a trailing `&` would background the whole && chain, $L included.
            "mkdir -p ~/.cache/yunpao; L=~/.cache/yunpao/pip-$(date +%s).log; \
             nohup sh -c {} sh {} {pkgs} > \"$L\" 2>&1 < /dev/null & tail -n +1 -f --pid=$! \"$L\"",
            shlex::try_quote(&inner).unwrap(),
            shlex::try_quote(&py).unwrap(),
        );
        let _ = tx.send(ev("line", serde_json::json!({ "text": format!("$ {py} -m pip install {pkgs}") })));
        let mut args = ssh_base_args(server);
        args.push(cmd);
        let child = tokio::process::Command::new("ssh")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child { Ok(c) => c, Err(e) => return fail(&e.to_string()) };
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut code = None;
        while let Ok(Some(l)) = lines.next_line().await {
            if let Some(rc) = l.strip_prefix(RC_MARK) {
                code = rc.trim().parse::<i32>().ok();
            } else if tx.send(ev("line", serde_json::json!({ "text": l }))).is_err() {
                return; // page gone; the install keeps running on the server
            }
        }
        let status = child.wait().await;
        match code {
            Some(c) => { let _ = tx.send(ev("done", serde_json::json!({ "ok": c == 0, "code": c }))); }
            None if status.is_ok_and(|s| s.code() == Some(255)) => fail("SSH 连接失败，先确认能连上服务器"),
            None => fail("安装中断了，没拿到 pip 的结果"),
        }
    });
    sse_from(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_imports_and_literals() {
        let src = "import os, torch.nn as nn\nfrom PIL import Image\nfrom .local import x\nimport numpy as np  # comment\n";
        assert_eq!(imports(src).into_iter().collect::<Vec<_>>(), vec!["PIL", "numpy", "os", "torch"]);
        assert_eq!(literals("df = pd.read_csv('data/train.csv')  # 'not this'"), vec!["data/train.csv"]);
        assert_eq!(literals(r#"open("a\"b.txt", "r")"#), vec![r#"a\"b.txt"#, "r"]);
    }

    #[test]
    fn pip_names_only_when_sure() {
        assert_eq!(pip_name("whisper"), Some("openai-whisper"));
        assert_eq!(pip_name("cv2"), Some("opencv-python"));
        assert_eq!(pip_name("numpy"), Some("numpy"));
        assert_eq!(pip_name("clip"), None);
        assert_eq!(pip_name("utils"), None);
    }

    #[test]
    fn estimates_model_memory() {
        assert_eq!(model_params("Qwen/Qwen2.5-7B-Instruct"), Some(7.0));
        assert_eq!(model_params("meta-llama/Llama-2-13b-hf"), Some(13.0));
        assert_eq!(model_params("Qwen/Qwen2-1.5B"), Some(1.5));
        assert_eq!(model_params("bert-base-uncased"), None);
        assert_eq!(model_params("data/train.csv"), None);
        let (inf, _) = estimate_gib(7.0, "model = AutoModel.from_pretrained(x)");
        let (lora, how) = estimate_gib(7.0, "LoraConfig(r=8)\ntrainer = Trainer(");
        let (full, _) = estimate_gib(7.0, "loss.backward()");
        assert!((17.0..18.0).contains(&inf) && how == "LoRA 微调" && lora < 25.0 && full > 130.0);
    }

    #[test]
    fn parses_progress() {
        let tq = "Epoch 1:  45%|████▌     | 450/1000 [01:23<01:40,  5.40it/s]\n";
        assert_eq!(parse_progress(tq, 100), Some(Progress { done: 450, total: 1000, unit: "step", eta_secs: Some(100) }));
        let ep = "loading\nEpoch [3/10] loss=0.52\nstep 20 loss 0.4\n";
        assert_eq!(parse_progress(ep, 600), Some(Progress { done: 3, total: 10, unit: "epoch", eta_secs: Some(2400) }));
        assert_eq!(epoch_line("epoch 2 of 5"), Some((2, 5)));
        assert_eq!(epoch_line("epochs: 50"), None);
        assert_eq!(tqdm_line("  0%|          | 0/100 [00:00<?, ?it/s]"), Some((0, 100, None)));
        assert_eq!(parse_progress("hello\nworld\n", 10), None);
    }

    #[test]
    fn parses_pmon() {
        let out = "# gpu         pid   type     sm    mem    enc    dec    jpg    ofa     fb   ccpm    command \n\
                   # Idx           #    C/G      %      %      %      %      %      %     MB     MB    name \n\
                   \x20   0          -     -      -      -      -      -      -      -      -      -    -   \n\
                   \x20   2    2913508     C     19     10      -      -      -      -  37026      0    python3 \n\
                   \x20   4    2678189   C+G      -      -      -      -      -      -   6936      0    python \n";
        assert_eq!(parse_pmon(out), vec![(2913508, 2, 37026, Some(19)), (2678189, 4, 6936, None)]);
    }
}
