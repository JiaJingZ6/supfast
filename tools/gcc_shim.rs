// gcc.exe 垫片 —— 只实现 windres 需要的那一步预处理
//
// 背景：便携 Rust（x86_64-pc-windows-gnu）里的 gcc 只是个链接器壳子，
// 不能编译/预处理 C 文件；而 windres 编译 .rc 时固定调用 `gcc -E`。
// tauri-build 生成的 resource.rc 里没有 #include，只有一行
// `#pragma code_page(65001)`，所以把 #pragma 行滤掉原样吐出来就够了。
//
// 注意：绝不能叫 x86_64-w64-mingw32-gcc.exe —— rustc 用那个名字做链接，
// 名字叫 gcc.exe 就只会被 windres 撞上。

use std::io::{self, Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has_e = args.iter().any(|a| a == "-E");
    if !has_e {
        eprintln!("gcc-shim: 只支持 -E（windres 预处理）");
        std::process::exit(2);
    }

    // 带参数的选项要整体跳过，别把 "RC_INVOKED" 当成输入文件
    const WITH_ARG: &[&str] = &[
        "-o", "-I", "-D", "-U", "-x", "-include", "-imacros", "-isystem", "-idirafter",
        "-MF", "-MT", "-MQ", "-std", "-iprefix", "-iwithprefix",
    ];
    let mut out: Option<String> = None;
    let mut input: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-o" {
            if i + 1 < args.len() {
                out = Some(args[i + 1].clone());
                i += 2;
                continue;
            }
        } else if WITH_ARG.contains(&a.as_str()) {
            i += 2;
            continue;
        } else if !a.starts_with('-') {
            input = Some(a.clone());
        }
        i += 1;
    }

    let mut src = String::new();
    let ok = match input.as_deref() {
        Some("-") | None => io::stdin().read_to_string(&mut src).is_ok(),
        Some(p) => std::fs::read_to_string(p).map(|s| { src = s; }).is_ok(),
    };
    if !ok {
        eprintln!("gcc-shim: 读不到输入");
        std::process::exit(1);
    }

    let mut body = String::with_capacity(src.len());
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("#pragma") || t.starts_with("# include") || t.starts_with("#include") {
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }

    match out {
        Some(p) => {
            if std::fs::write(&p, body.as_bytes()).is_err() {
                eprintln!("gcc-shim: 写不了 {}", p);
                std::process::exit(1);
            }
        }
        None => {
            let stdout = io::stdout();
            let mut h = stdout.lock();
            let _ = h.write_all(body.as_bytes());
        }
    }
}
