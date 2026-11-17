// 开发用：从外部摆布 supfast 窗口（改尺寸 / 最小化别的窗口），方便截图核对排版。
// 用法：
//   winbox.exe resize 1980 1410 ["标题子串"]
//   winbox.exe min "Google Chrome"
// rustc -O --edition 2021 winbox.rs -o winbox.exe
#[repr(C)]
struct RECT {
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, p: isize) -> i32;
    fn IsWindowVisible(h: isize) -> i32;
    fn GetWindowTextLengthW(h: isize) -> i32;
    fn GetWindowTextW(h: isize, buf: *mut u16, n: i32) -> i32;
    fn GetWindowRect(h: isize, r: *mut RECT) -> i32;
    fn SetWindowPos(h: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn SetForegroundWindow(h: isize) -> i32;
    fn ShowWindow(h: isize, cmd: i32) -> i32;
    fn SetProcessDPIAware() -> i32;
    fn IsIconic(h: isize) -> i32;
    fn IsZoomed(h: isize) -> i32;
}

static mut FOUND: isize = 0;
static NEEDLE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

extern "system" fn cb(h: isize, _p: isize) -> i32 {
    unsafe {
        if IsWindowVisible(h) == 0 {
            return 1;
        }
        let n = GetWindowTextLengthW(h);
        if n < 1 {
            return 1;
        }
        let mut b = vec![0u16; (n + 2) as usize];
        GetWindowTextW(h, b.as_mut_ptr(), n + 1);
        let title = String::from_utf16_lossy(&b[..n as usize]);
        let needle = NEEDLE.lock().unwrap().clone();
        if title.to_lowercase().contains(&needle.to_lowercase()) {
            println!("hit hwnd={} title={}", h, title);
            FOUND = h;
            return 0;
        }
        1
    }
}

fn main() {
    unsafe { SetProcessDPIAware() };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().cloned().unwrap_or_default();
    if mode == "resize" {
        let w: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1980);
        let h: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1410);
        *NEEDLE.lock().unwrap() = args.get(3).cloned().unwrap_or_else(|| "supfast".into());
        unsafe { EnumWindows(cb, 0) };
        let hwnd = unsafe { FOUND };
        if hwnd == 0 {
            println!("window not found");
            return;
        }
        let mut r = RECT { l: 0, t: 0, r: 0, b: 0 };
        unsafe { GetWindowRect(hwnd, &mut r) };
        println!("before: ({},{}) {}x{}", r.l, r.t, r.r - r.l, r.b - r.t);
        unsafe {
            SetForegroundWindow(hwnd);
            SetWindowPos(hwnd, 0, r.l, r.t, w, h, 0x0014);
        }
        std::thread::sleep(std::time::Duration::from_millis(350));
        let mut r2 = RECT { l: 0, t: 0, r: 0, b: 0 };
        unsafe { GetWindowRect(hwnd, &mut r2) };
        println!(
            "after:  ({},{}) {}x{}",
            r2.l,
            r2.t,
            r2.r - r2.l,
            r2.b - r2.t
        );
    } else if mode == "state" {
        *NEEDLE.lock().unwrap() = args.get(1).cloned().unwrap_or_else(|| "supfast".into());
        unsafe { EnumWindows(cb, 0) };
        let h = unsafe { FOUND };
        if h == 0 {
            println!("window not found（可能已关闭）");
            return;
        }
        unsafe {
            println!("hwnd={} minimized={} maximized={}", h, IsIconic(h), IsZoomed(h));
        }
    } else if mode == "min" {
        *NEEDLE.lock().unwrap() = args.get(1).cloned().unwrap_or_default();
        unsafe { EnumWindows(cb, 0) };
        let hits = unsafe { FOUND };
        if hits != 0 {
            unsafe { ShowWindow(hits, 6) };
        }
        println!("minimized");
    } else {
        println!("usage: winbox resize <w> <h> [title] | winbox min <title>");
    }
}
