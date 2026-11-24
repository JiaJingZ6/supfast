//! SUPer 可选后端 —— 只调用用户自备的 CLI，不随包分发
//!
//! `cubicibo/SUPer` 是完整的 HDMV PGS 编码器，吃 BDN XML + PNG，吐合规的 SUP / PES+MUI。
//! 它是 GPL-3.0-or-later，且运行需要 Python ≥3.12 + numpy / opencv / pyopencl /
//! brulegfx 等一整套（独立包约 100 MB）。许可与体积都不允许塞进 supfast 的 3 MB 包里，
//! 所以这里**不链接、不内嵌、不复制**它的任何代码，只做「探测 + 外部进程调用」，
//! 由用户自行安装并指定 `supercli.py`（或一个自己写的启动器 .bat/.cmd/.exe）。
//!
//! 探测优先级：
//!   1. 界面 / CLI 传入的 `super_cmd`
//!   2. 环境变量 `SUPFAST_SUPER_CMD`
//!   3. 可执行文件同目录的 `SUPer\supercli.py`、`supercli.py`
//!   4. `%APPDATA%\supfast-desktop\supercli.py`
//!   5. PATH 里的 `supercli.py` / `supercli.exe` / `supercli`
//! 目标是 `.py` 时再找解释器：`SUPFAST_SUPER_PY` → 脚本旁的 venv → PATH 的 python。
//!
//! 为什么单独重定向到日志文件：SUPer 编码时进度输出不少，用管道却不读会把子进程
//! 卡在写满的缓冲区上；而且日志留在输出目录里，用户能直接回看量化/丢帧决策。

use serde_json::{json, Value};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// GUI 进程 spawn 控制台程序必须带这个，否则每段都弹一个 conhost
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 解析后的一次可执行调用
pub struct SuperCmd {
    pub program: String,
    pub pre: Vec<String>,
    pub label: String,
}

fn is_script(p: &Path) -> bool {
    p.extension()
        .map(|e| e.eq_ignore_ascii_case("py"))
        .unwrap_or(false)
}

/// 在 PATH 里找可执行名（Windows 顺带试 .exe）
fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&paths) {
        let p = d.join(name);
        if p.is_file() {
            return Some(p);
        }
        let e = d.join(format!("{}.exe", name));
        if e.is_file() {
            return Some(e);
        }
    }
    None
}

fn find_entry(configured: &str) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    let t = configured.trim();
    if !t.is_empty() {
        cands.push(PathBuf::from(t));
    }
    if let Ok(v) = std::env::var("SUPFAST_SUPER_CMD") {
        if !v.trim().is_empty() {
            cands.push(PathBuf::from(v.trim()));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("SUPer").join("supercli.py"));
            cands.push(dir.join("supercli.py"));
        }
    }
    if let Ok(app) = std::env::var("APPDATA") {
        cands.push(PathBuf::from(app).join("supfast-desktop").join("supercli.py"));
    }
    for c in &cands {
        if c.is_file() {
            return Some(c.clone());
        }
    }
    for n in ["supercli.py", "supercli.exe", "supercli"] {
        if let Some(p) = which(n) {
            return Some(p);
        }
    }
    None
}

fn find_python(script: &Path) -> Option<String> {
    if let Ok(v) = std::env::var("SUPFAST_SUPER_PY") {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    if let Some(dir) = script.parent() {
        for sub in [
            "venv/Scripts/python.exe",
            ".venv/Scripts/python.exe",
            "Scripts/python.exe",
        ] {
            let p = dir.join(sub);
            if p.is_file() {
                return Some(p.to_string_lossy().to_string());
            }
        }
    }
    if let Some(p) = which("python.exe").or_else(|| which("python")) {
        return Some(p.to_string_lossy().to_string());
    }
    // 交给系统解析，运行时若不存在会在 spawn 时明确报错
    Some("python".into())
}

/// 把「用户填的路径」解析成一条真正能 spawn 的命令
pub fn resolve(configured: &str) -> Result<SuperCmd, String> {
    let entry = find_entry(configured).ok_or_else(|| {
        "未找到 SUPer CLI：请在高级选项里填 supercli.py 路径，或设环境变量 SUPFAST_SUPER_CMD"
            .to_string()
    })?;
    if is_script(&entry) {
        let py = find_python(&entry).ok_or("找不到 Python 解释器来运行 supercli.py")?;
        let script = fs::canonicalize(&entry).unwrap_or_else(|_| entry.clone());
        let s = script.to_string_lossy().to_string();
        Ok(SuperCmd {
            label: format!("{} · {}", Path::new(&py).file_name().map(|x| x.to_string_lossy()).unwrap_or_default(), s),
            program: py,
            pre: vec![s],
        })
    } else {
        let p = entry.to_string_lossy().to_string();
        Ok(SuperCmd {
            label: p.clone(),
            program: p,
            pre: Vec::new(),
        })
    }
}

/// 给「环境」页用：只检查文件是否存在，不真的启动 Python（避免每次开界面都卡一下）
pub fn probe(configured: &str) -> (bool, String) {
    match resolve(configured) {
        Ok(c) => (true, c.label),
        Err(e) => (false, e),
    }
}

fn tail(path: &Path, max_lines: usize) -> String {
    let raw = fs::read(path).unwrap_or_default();
    let s = String::from_utf8_lossy(&raw);
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn failed(error: impl Into<String>, log_path: &Path, cmdline: &str) -> Value {
    json!({
        "ok": false,
        "error": error.into(),
        "log": tail(log_path, 12),
        "log_path": log_path.to_string_lossy(),
        "cmd": cmdline,
    })
}

/// 用 SUPer 把 BDN XML 重新编码成合规 SUP。失败只写进返回值，绝不让整个 convert 失败。
pub fn encode(cmd: &SuperCmd, xml: &Path, out: &Path, cancel: &AtomicBool) -> Value {
    let started = Instant::now();
    // `<名>.super.sup` → `<名>.super.log`
    let log_path = out.with_extension("log");
    // Windows 上实测 -t 8 会让 SUPer 的多进程 companion worker 全部 30s 超时
    // （"Companion worker did not start."）；单线程编 129 个事件只用 4.8 秒，足够。
    // 高级用户可用 SUPFAST_SUPER_THREADS 覆盖。
    let threads = std::env::var("SUPFAST_SUPER_THREADS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|n| n.clamp(1, 10).to_string())
        .unwrap_or_else(|| "1".to_string());

    let mut c = Command::new(&cmd.program);
    c.args(&cmd.pre)
        // Qtzr 是 SUPer 内置量化器，不依赖外部 pngquant / libimagequant，
        // 免得用户还要额外配 config.ini；质量也最好（代价是体积略大）。
        .arg("-i").arg(xml)
        .arg("-q").arg("0")
        .arg("-t").arg(&threads)
        .arg("-y")
        .arg(out);
    if let Some(dir) = out.parent() {
        c.current_dir(dir);
    }
    let cmdline = format!(
        "{} {} -i {} -q 0 -t {} -y {}",
        cmd.program,
        cmd.pre.join(" "),
        xml.display(),
        threads,
        out.display()
    );

    let log_file = match File::create(&log_path) {
        Ok(f) => f,
        Err(e) => return failed(format!("建 SUPer 日志失败: {}", e), &log_path, &cmdline),
    };
    let log_file2 = match log_file.try_clone() {
        Ok(f) => f,
        Err(e) => return failed(format!("建 SUPer 日志失败: {}", e), &log_path, &cmdline),
    };
    c.stdout(Stdio::from(log_file)).stderr(Stdio::from(log_file2));
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);

    let mut child = match c.spawn() {
        Ok(x) => x,
        Err(e) => {
            return failed(
                format!("启动 SUPer 失败: {}（检查 Python 环境与 supercli.py 路径）", e),
                &log_path,
                &cmdline,
            )
        }
    };

    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if cancel.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return failed("已取消", &log_path, &cmdline);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return failed(format!("等待 SUPer 失败: {}", e), &log_path, &cmdline),
        }
    };
    // 子进程已退出，再读日志，保证拿到完整输出
    let log = tail(&log_path, 12);
    let size = fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    let ok = status.success() && size > 0;
    let err = if !status.success() {
        Some(format!("SUPer 退出码 {:?}", status.code()))
    } else if size == 0 {
        Some("SUPer 未产出 .sup 文件".to_string())
    } else {
        None
    };

    let mut v = json!({
        "ok": ok,
        "path": out.to_string_lossy(),
        "size": size,
        "secs": (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
        "log": log,
        "log_path": log_path.to_string_lossy(),
        "cmd": cmdline,
    });
    if let Some(e) = err {
        v["error"] = json!(e);
    }
    v
}
