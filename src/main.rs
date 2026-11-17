// supfast —— ASS 字幕转 PGS（Tauri 版）
//
// 前端是 ui/index.html（和 Electron 版同一份界面），通过 window.__TAURI__ 的
// invoke 走下面这个统一的 api 路由；引擎在 engine.rs 里，纯 Rust，零外部依赖。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};

// ---------------------------------------------------------------------------
// 共享状态
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Job {
    running: bool,
    stage: String,
    pct: f64,
    done: usize,
    total: usize,
    pngs: usize,
    result: Option<Value>,
    error: Option<String>,
    started: Option<std::time::Instant>,
    plan: Option<Value>,
}

#[derive(Clone)]
struct Shared {
    settings: Arc<Mutex<Value>>,
    recent: Arc<Mutex<Vec<Value>>>,
    history: Arc<Mutex<Vec<Value>>>,
    job: Arc<Mutex<Job>>,
    cancel: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<Value>>>,
    seq: Arc<AtomicUsize>,
    file: Arc<Mutex<PathBuf>>,
}

const DEFAULT_SETTINGS: &str = r#"{
  "mod_root":"","fps":"23.976","vfmt":"1080p","lang":"zho","title":"Undefined",
  "theme":"auto","jobs":8,"outdir":"",
  "palette":1,"autocrop":1,"bufsplit":1,"stricter":0,"even_y":0,"no_sup":0,"dry_run":0
}"#;

impl Shared {
    fn new(state_file: PathBuf) -> Self {
        let mut settings: Value = serde_json::from_str(DEFAULT_SETTINGS).unwrap();
        let mut recent: Vec<Value> = Vec::new();
        let mut history: Vec<Value> = Vec::new();

        if let Ok(raw) = std::fs::read_to_string(&state_file) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(obj) = v.get("settings").and_then(|s| s.as_object()) {
                    for (k, val) in obj {
                        settings[k] = val.clone();
                    }
                }
                if let Some(arr) = v.get("recent").and_then(|x| x.as_array()) {
                    recent = arr.iter().take(12).cloned().collect();
                }
                if let Some(arr) = v.get("history").and_then(|x| x.as_array()) {
                    history = arr.iter().take(40).cloned().collect();
                }
            }
        }

        Shared {
            settings: Arc::new(Mutex::new(settings)),
            recent: Arc::new(Mutex::new(recent)),
            history: Arc::new(Mutex::new(history)),
            job: Arc::new(Mutex::new(Job::default())),
            cancel: Arc::new(AtomicBool::new(false)),
            log: Arc::new(Mutex::new(Vec::new())),
            seq: Arc::new(AtomicUsize::new(0)),
            file: Arc::new(Mutex::new(state_file)),
        }
    }

    fn save(&self) {
        let path = self.file.lock().unwrap().clone();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = json!({
            "settings": self.settings.lock().unwrap().clone(),
            "recent": self.recent.lock().unwrap().clone(),
            "history": self.history.lock().unwrap().clone(),
        });
        if let Ok(s) = serde_json::to_string_pretty(&body) {
            let _ = std::fs::write(&path, s);
        }
    }

    fn push_log(&self, line: String, kind: &str) {
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut g = self.log.lock().unwrap();
        g.push(json!({ "seq": n, "line": line, "kind": kind }));
        if g.len() > 3000 {
            let cut = g.len() - 2000;
            g.drain(0..cut);
        }
    }

    fn clear_log(&self) {
        self.log.lock().unwrap().clear();
        self.seq.store(0, Ordering::SeqCst);
    }

    fn mod_root(&self) -> Option<String> {
        let s = self.settings.lock().unwrap();
        s.get("mod_root")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string())
            .filter(|x| !x.is_empty())
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn fmt_n(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{}", out) } else { out }
}

fn file_name(p: &str) -> String {
    Path::new(p)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string())
}

/// 记住最近打开的字幕（按规范化路径去重）
fn remember(sh: &Shared, path: &str) {
    if path.is_empty() {
        return;
    }
    let name = file_name(path);
    let key = path.replace('\\', "/").to_lowercase();
    let mut r = sh.recent.lock().unwrap();
    r.retain(|x| {
        x.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.replace('\\', "/").to_lowercase() != key)
            .unwrap_or(true)
    });
    r.insert(0, json!({ "path": path, "name": name }));
    r.truncate(12);
}

// ---------------------------------------------------------------------------
// 路由
// ---------------------------------------------------------------------------

fn status_of(sh: &Shared, since: usize) -> Value {
    let (running, stage, pct, done, total, pngs, result, error, plan, elapsed) = {
        let j = sh.job.lock().unwrap();
        (
            j.running,
            j.stage.clone(),
            j.pct,
            j.done,
            j.total,
            j.pngs,
            j.result.clone(),
            j.error.clone(),
            j.plan.clone(),
            j.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0),
        )
    };
    let lines: Vec<Value> = sh
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.get("seq").and_then(|s| s.as_u64()).unwrap_or(0) as usize > since)
        .cloned()
        .collect();
    json!({
        "ok": true,
        "running": running,
        "stage": stage,
        "pct": pct,
        "done": done,
        "total": total,
        "pngs": pngs,
        "result": result,
        "error": error,
        "plan": plan,
        "elapsed": elapsed,
        "lines": lines,
        "log_seq": sh.seq.load(Ordering::SeqCst),
    })
}

fn route(sh: &Shared, route: &str, body: &Value) -> Result<Value, String> {
    let path = route.split('?').next().unwrap_or(route);
    match path {
        // ---------------- 首屏 ----------------
        "/api/boot" => {
            let env = engine::probe_env(sh.mod_root().as_deref());
            Ok(json!({
                "ok": true,
                "settings": sh.settings.lock().unwrap().clone(),
                "recent": sh.recent.lock().unwrap().clone(),
                "history": sh.history.lock().unwrap().clone(),
                "env": env.get("items").cloned().unwrap_or(json!([])),
                "mod_dir": env.get("mod_dir").cloned().unwrap_or(json!("")),
                "version": env!("CARGO_PKG_VERSION"),
                "tauri": "2",
                "autostart": std::env::var("SUPFAST_AUTOSTART").map(|v| v == "1").unwrap_or(false),
                "pane": std::env::var("SUPFAST_PANE").unwrap_or_default(),
                // 调试：SUPFAST_OPEN_ADV=1 时展开「高级选项」，方便截图核对开关排版
                "openAdv": std::env::var("SUPFAST_OPEN_ADV").map(|v| v == "1").unwrap_or(false),
            }))
        }

        // ---------------- 设置 ----------------
        "/api/settings" => {
            if let Some(new) = body.get("settings").and_then(|s| s.as_object()) {
                let mut s = sh.settings.lock().unwrap();
                for (k, v) in new {
                    s[k] = v.clone();
                }
            }
            sh.save();
            let env = engine::probe_env(sh.mod_root().as_deref());
            Ok(json!({
                "ok": true,
                "env": env.get("items").cloned().unwrap_or(json!([])),
                "mod_dir": env.get("mod_dir").cloned().unwrap_or(json!("")),
            }))
        }

        "/api/state" => {
            Ok(json!({
                "ok": true,
                "recent": sh.recent.lock().unwrap().clone(),
                "history": sh.history.lock().unwrap().clone(),
                "settings": sh.settings.lock().unwrap().clone(),
            }))
        }

        // ---------------- 分析 ----------------
        "/api/analyze" => {
            let r = engine::analyze(body)?;
            remember(sh, body.get("ass").and_then(|a| a.as_str()).unwrap_or(""));
            sh.save();
            Ok(r)
        }

        // ---------------- 开跑 ----------------
        "/api/start" => {
            if sh.job.lock().unwrap().running {
                return Err("已有任务在跑".into());
            }
            let opts = body.get("opts").cloned().unwrap_or_else(|| json!({}));
            let ass = opts.get("ass").and_then(|a| a.as_str()).unwrap_or("").to_string();
            if ass.is_empty() || !Path::new(&ass).is_file() {
                return Err("请先选择 .ass 字幕".into());
            }
            if let Some(obj) = opts.as_object() {
                let mut s = sh.settings.lock().unwrap();
                for (k, v) in obj {
                    s[k] = v.clone();
                }
            }
            remember(sh, &ass);
            sh.save();

            sh.clear_log();
            sh.cancel.store(false, Ordering::SeqCst);
            {
                let mut j = sh.job.lock().unwrap();
                *j = Job {
                    running: true,
                    stage: "plan".into(),
                    pct: 1.0,
                    started: Some(std::time::Instant::now()),
                    ..Job::default()
                };
            }

            let fps = engine::pick_f64(&opts, "fps");
            sh.push_log(
                format!("supfast {} · {} → PGS", env!("CARGO_PKG_VERSION"), file_name(&ass)),
                "cmd",
            );
            sh.push_log(
                format!(
                    "参数: {:.3}fps / {} / 并行 {} / lang={} / track={}",
                    if fps > 1.0 { fps } else { 23.976 },
                    opts.get("vfmt").and_then(|v| v.as_str()).unwrap_or("1080p"),
                    engine::pick_u32(&opts, "jobs"),
                    opts.get("lang").and_then(|v| v.as_str()).unwrap_or("und"),
                    opts.get("title").and_then(|v| v.as_str()).unwrap_or("Undefined"),
                ),
                "dim",
            );

            let sh2 = sh.clone();
            std::thread::spawn(move || {
                let sh3 = sh2.clone();
                let emit: engine::Emit = Arc::new(move |ev: Value| {
                    let t = ev.get("t").and_then(|x| x.as_str()).unwrap_or("");
                    match t {
                        "stage" => {
                            let name = ev.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            let mut j = sh3.job.lock().unwrap();
                            if name == "plan" {
                                j.stage = "plan".into();
                            } else if name == "render" {
                                j.stage = "render".into();
                                j.total = ev.get("total").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                                j.pct = 3.0;
                                drop(j);
                                sh3.push_log("逐段调用 avs2bdnxml…".into(), "dim");
                            } else if name == "write" {
                                j.stage = "write".into();
                                j.pct = 96.0;
                                drop(j);
                                sh3.push_log("重写 XML / 重映射 PTS…".into(), "dim");
                            }
                        }
                        "seg" => {
                            let done = ev.get("done").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                            let total = ev.get("total").and_then(|x| x.as_u64()).unwrap_or(1) as usize;
                            let pngs = ev.get("pngs").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                            {
                                let mut j = sh3.job.lock().unwrap();
                                j.done = done;
                                j.total = total;
                                j.pngs = pngs;
                                j.pct = 3.0 + 92.0 * (done as f64 / total.max(1) as f64);
                            }
                            if done % 50 == 0 || done == total {
                                sh3.push_log(
                                    format!("  [{}/{}] {} 张 PNG", done, total, fmt_n(pngs as i64)),
                                    "dim",
                                );
                            }
                        }
                        "warn" => {
                            let msg = ev.get("msg").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            sh3.push_log(format!("[warn] {}", msg), "err");
                        }
                        _ => {}
                    }
                });

                let res = engine::convert(&opts, emit, sh2.cancel.clone());
                {
                    let mut j = sh2.job.lock().unwrap();
                    j.running = false;
                }
                match res {
                    Ok(mut v) => {
                        let secs = v.get("secs").and_then(|x| x.as_f64()).unwrap_or(0.0);
                        let pngs = v.get("pngs").and_then(|x| x.as_u64()).unwrap_or(0);
                        let speed = v.get("speed").and_then(|x| x.as_f64()).unwrap_or(0.0);
                        let failed = v.get("failed").and_then(|x| x.as_u64()).unwrap_or(0);
                        v["ts"] = json!(std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs_f64())
                            .unwrap_or(0.0));
                        v["name"] = json!(file_name(
                            opts.get("ass").and_then(|a| a.as_str()).unwrap_or("")
                        ));
                        v["ass"] = json!(opts.get("ass").and_then(|a| a.as_str()).unwrap_or(""));
                        sh2.push_log(
                            format!(
                                "完成: {} 张 PNG / {:.1} 秒 / {}x",
                                fmt_n(pngs as i64),
                                secs,
                                if speed > 0.0 { format!("{:.2}", speed) } else { "-".into() }
                            ),
                            "done",
                        );
                        if failed > 0 {
                            sh2.push_log(format!("[warn] 有 {} 段渲染失败，PNG 会缺", failed), "err");
                        }
                        {
                            let mut j = sh2.job.lock().unwrap();
                            j.stage = "done".into();
                            j.pct = 100.0;
                            j.result = Some(v.clone());
                        }
                        {
                            let mut h = sh2.history.lock().unwrap();
                            h.insert(0, v);
                            h.truncate(24);
                        }
                        sh2.save();
                    }
                    Err(e) => {
                        let cancelled = sh2.cancel.load(Ordering::SeqCst);
                        sh2.push_log(
                            format!("[error] {}", e),
                            if cancelled { "dim" } else { "err" },
                        );
                        let mut j = sh2.job.lock().unwrap();
                        j.stage = if cancelled { "cancel".into() } else { "error".into() };
                        j.pct = 0.0;
                        j.error = if cancelled { None } else { Some(e) };
                    }
                }
            });

            Ok(json!({ "ok": true }))
        }

        "/api/cancel" => {
            sh.cancel.store(true, Ordering::SeqCst);
            sh.push_log("收到取消请求…".into(), "dim");
            Ok(json!({ "ok": true }))
        }

        "/api/status" => {
            let since = route
                .split("since=")
                .nth(1)
                .and_then(|s| s.split('&').next())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            Ok(status_of(sh, since))
        }

        // ---------------- 历史 / 打开目录 ----------------
        "/api/forget" => {
            let kind = body.get("kind").and_then(|x| x.as_str()).unwrap_or("");
            match kind {
                "history" => {
                    let ts = body.get("ts").and_then(|x| x.as_f64()).unwrap_or(-1.0);
                    sh.history.lock().unwrap().retain(|h| {
                        (h.get("ts").and_then(|t| t.as_f64()).unwrap_or(-2.0) - ts).abs() > 1e-6
                    });
                }
                "history-all" => sh.history.lock().unwrap().clear(),
                "recent" => {
                    let p = body.get("path").and_then(|x| x.as_str()).unwrap_or("");
                    sh.recent
                        .lock()
                        .unwrap()
                        .retain(|r| r.get("path").and_then(|x| x.as_str()).unwrap_or("") != p);
                }
                _ => {}
            }
            sh.save();
            Ok(json!({ "ok": true }))
        }

        "/api/open" => {
            let p = body.get("path").and_then(|x| x.as_str()).unwrap_or("");
            if p.is_empty() || !Path::new(p).exists() {
                return Err("路径不存在".into());
            }
            let mode = body.get("mode").and_then(|x| x.as_str()).unwrap_or("file");
            let mut cmd = std::process::Command::new("explorer");
            if mode == "dir" {
                cmd.arg(p);
            } else {
                cmd.arg("/select,").arg(p);
            }
            let _ = cmd.spawn();
            Ok(json!({ "ok": true }))
        }

        _ => Err(format!("未知路由 {}", path)),
    }
}

#[tauri::command]
fn api(sh: State<'_, Shared>, path: String, body: Value) -> Result<Value, String> {
    route(&sh, &path, &body)
}

// ---------------------------------------------------------------------------
// 引擎自检（不进 GUI，方便和 JS 版对拍）
// ---------------------------------------------------------------------------

/// release 版是 windows 子系统，没有 stdout；给 --json 就写文件
fn emit_json(s: &str, path: &str) {
    if path.is_empty() {
        println!("{}", s);
    } else {
        let _ = std::fs::write(path, s);
    }
}

fn cli_engine(args: &[String]) -> i32 {
    let mut mode = "analyze";
    let mut ass = String::new();
    let mut out = String::new();
    let mut fps = 23.976_f64;
    let mut jobs = 8_u32;
    let mut json_out = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "analyze" => mode = "analyze",
            "convert" => mode = "convert",
            "--ass" => {
                i += 1;
                ass = args.get(i).cloned().unwrap_or_default();
            }
            "--out" => {
                i += 1;
                out = args.get(i).cloned().unwrap_or_default();
            }
            "--fps" => {
                i += 1;
                fps = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(23.976);
            }
            "--jobs" => {
                i += 1;
                jobs = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(8);
            }
            "--json" => {
                i += 1;
                json_out = args.get(i).cloned().unwrap_or_default();
            }
            _ => {}
        }
        i += 1;
    }
    if ass.is_empty() {
        eprintln!("用法: supfast --engine analyze|convert --ass <file.ass> [--out <dir>] [--fps 23.976] [--jobs 8]");
        return 2;
    }
    let opts = json!({ "ass": ass, "fps": fps, "jobs": jobs, "outdir": out,
        "vfmt": "1080p", "lang": "chi", "title": "Undefined" });
    match mode {
        "analyze" => match engine::analyze(&opts) {
            Ok(v) => {
                let brief = json!({
                    "events": v["events"], "frames": v["frames"], "segs": v["segs"],
                    "sta": v["sta"], "dyn": v["dyn"], "work": v["work"], "ratio": v["ratio"],
                    "w": v["w"], "h": v["h"],
                    "bands_len": v["bands"].as_array().map(|a| a.len()).unwrap_or(0),
                    "timeline_len": v["timeline"].as_array().map(|a| a.len()).unwrap_or(0),
                });
                emit_json(&serde_json::to_string_pretty(&brief).unwrap(), &json_out);
                0
            }
            Err(e) => {
                eprintln!("analyze 失败: {}", e);
                1
            }
        },
        _ => {
            let emit: engine::Emit = Arc::new(|ev: Value| {
                if ev.get("t").and_then(|t| t.as_str()) == Some("seg") {
                    let d = ev.get("done").and_then(|x| x.as_u64()).unwrap_or(0);
                    let t = ev.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
                    if d % 25 == 0 || d == t {
                        eprintln!("  [{}/{}]", d, t);
                    }
                }
            });
            match engine::convert(&opts, emit, Arc::new(AtomicBool::new(false))) {
                Ok(v) => {
                    emit_json(&serde_json::to_string_pretty(&v).unwrap(), &json_out);
                    0
                }
                Err(e) => {
                    eprintln!("convert 失败: {}", e);
                    1
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|a| a == "--engine").unwrap_or(false) {
        std::process::exit(cli_engine(&args[1..]));
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // 沿用 Electron 版的位置（%APPDATA%\supfast-desktop），历史与最近直接继承
            let dir = std::env::var("APPDATA")
                .map(|a| PathBuf::from(a).join("supfast-desktop"))
                .unwrap_or_else(|_| {
                    app.path()
                        .app_data_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                });
            app.manage(Shared::new(dir.join("state.json")));

            // 关掉 WebView2 的默认右键菜单（刷新 / 另存为 / 打印 / 检查）。
            // 页面里也拦了 contextmenu，但 WebView2 的菜单是宿主进程画的，
            // 只有 AreDefaultContextMenusEnabled=false 才是权威开关。
            if let Some(win) = app.get_webview_window("main") {
                let r = win.with_webview(|w| unsafe {
                    let mut problem = String::new();
                    match w.controller().CoreWebView2() {
                        Ok(core) => match core.Settings() {
                            Ok(settings) => {
                                if let Err(e) = settings.SetAreDefaultContextMenusEnabled(false) {
                                    problem = format!("set failed: {e}");
                                } else {
                                    // 回读确认：BOOL 是 repr(transparent) 的 i32，直接按 i32 接
                                    let mut on: i32 = -1;
                                    let ok = settings
                                        .AreDefaultContextMenusEnabled(&mut on as *mut i32 as _)
                                        .is_ok();
                                    if ok && on != 0 {
                                        problem = "still enabled after set".into();
                                    }
                                }
                            }
                            Err(e) => problem = format!("settings failed: {e}"),
                        },
                        Err(e) => problem = format!("core failed: {e}"),
                    }
                    if !problem.is_empty() {
                        // 只在失败时落一个诊断文件，正常情况什么都不写
                        let p = std::env::temp_dir().join("supfast-ctxmenu.log");
                        let _ = std::fs::write(p, problem);
                    }
                });
                if let Err(e) = r {
                    let p = std::env::temp_dir().join("supfast-ctxmenu.log");
                    let _ = std::fs::write(p, format!("with_webview failed: {e}"));
                }
            }

            // 调试：SUPFAST_EVAL 里的 JS 在界面加载后跑一次，结果自己写进 DOM 再截图看
            if let Ok(js) = std::env::var("SUPFAST_EVAL") {
                if let Some(win) = app.get_webview_window("main") {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(1800));
                        if let Err(e) = win.eval(&js) {
                            let p = std::env::temp_dir().join("supfast-eval.log");
                            let _ = std::fs::write(p, format!("eval failed: {e}"));
                        }
                    });
                }
            }

            // 1320×940 在 1280×720 这类屏上会超出屏幕（右/下被裁掉），按显示器尺寸收一收
            if let Some(win) = app.get_webview_window("main") {
                if let Ok(Some(mon)) = app.primary_monitor() {
                    let scale = mon.scale_factor();
                    let sw = mon.size().width as f64 / scale;
                    let sh = mon.size().height as f64 / scale;
                    let want_w = 1320.0_f64.min(sw - 60.0).max(900.0);
                    let want_h = 940.0_f64.min(sh - 80.0).max(620.0);
                    if want_w < 1320.0 || want_h < 940.0 {
                        let _ = win.set_size(tauri::LogicalSize::new(want_w, want_h));
                        let _ = win.set_min_size(Some(tauri::LogicalSize::new(
                            want_w.min(1000.0),
                            want_h.min(640.0),
                        )));
                        let _ = win.center();
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![api])
        .run(tauri::generate_context!())
        .expect("tauri run");
}
