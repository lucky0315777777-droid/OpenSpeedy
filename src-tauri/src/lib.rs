pub mod applog;
mod bridge_client;
mod bridge_job;
mod process_enumerator;
mod system_stats;

use process_enumerator::ProcessInfo;
use process_enumerator::ModuleInfo;
use std::process::Child;
use std::sync::Mutex;

use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::Foundation::BOOL;

static BRIDGE_CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// Ctrl+C handler — ensures bridge processes are killed when the user
/// presses Ctrl+C in the terminal (debug mode / running from console).
unsafe extern "system" fn ctrlc_handler(_ctrl_type: u32) -> BOOL {
    disable_all_enabled();
    shutdown_bridges();
    std::process::exit(0);
}

fn ensure_bridges() {
    // A poisoned lock here used to panic the whole process; the bridge list is
    // bookkeeping, so recover the data instead.
    let mut children = match BRIDGE_CHILDREN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            applog::warn("bridge", "BRIDGE_CHILDREN mutex was poisoned, recovering");
            poisoned.into_inner()
        }
    };
    if !children.is_empty() {
        applog::info("bridge", "bridges already started — skipping");
        return;
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    applog::info("bridge", format!("looking for bridge binaries in {}", exe_dir.display()));

    for name in &["bridge64.exe", "bridge32.exe"] {
        let path = exe_dir.join(name);
        let log_path = exe_dir.join(format!("{name}.log"));
        if !path.exists() {
            applog::warn("bridge", format!("{name} not found at {} — skipped", path.display()));
            continue;
        }
        // Append instead of truncating: the old behaviour wiped the previous
        // session's bridge log on every start, which is exactly the session
        // you need after "it stopped working".
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map(std::process::Stdio::from)
            .unwrap_or_else(|e| {
                applog::warn("bridge", format!("cannot create {}: {e}", log_path.display()));
                std::process::Stdio::null()
            });
        match std::process::Command::new(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
        {
            Ok(mut child) => {
                applog::info("bridge", format!("spawned {} as pid {}", name, child.id()));
                // Join the cleanup job right away: the gap between spawn() and
                // this call is the only window in which a bridge could outlive a
                // force-killed app. Checked for liveness first — a bridge that
                // died on startup cannot be orphaned, and reporting "not in the
                // job" for it would mislead whoever reads the log.
                match child.try_wait() {
                    Ok(Some(status)) => applog::warn("bridge", format!(
                        "{name} (pid {}) exited immediately ({status}) — not added to the cleanup job",
                        child.id()
                    )),
                    _ => {
                        bridge_job::assign(&child, name);
                    }
                }
                children.push(child);
            }
            Err(e) => applog::error("bridge", format!("failed to spawn {}: {e}", path.display())),
        }
    }

    // Wait until bridges are ready (pipe server accepting connections)
    let health_checks: [(&str, fn() -> bool); 2] = [
        ("bridge64", bridge_client::bridge64_health as fn() -> bool),
        ("bridge32", bridge_client::bridge32_health as fn() -> bool),
    ];
    for (name, check) in &health_checks {
        let start = std::time::Instant::now();
        let mut ok = check();
        while !ok && start.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            ok = check();
        }
        if ok {
            applog::info("bridge", format!("{name} ready after {} ms", start.elapsed().as_millis()));
        } else {
            applog::error("bridge", format!("{name} did not become ready within 2 s — speed patching will not work"));
        }
    }
}

fn shutdown_bridges() {
    // Kill bridge processes immediately — graceful SHUTDOWN via pipe can
    // block if the bridge is busy processing a long-running command.
    //
    // This covers orderly exits only. A force-kill or a crash runs none of this,
    // and is handled by the `bridge_job` kill-on-close job instead. The job
    // handle is deliberately not closed here: `BRIDGE_JOB` is a `OnceLock`, so
    // closing would be irreversible and leave a dangling handle behind for any
    // later `ensure_bridges` to hand to `AssignProcessToJobObject`. There is
    // nothing to gain either — process exit closes it, which is exactly the
    // signal that tears the bridges down.
    if let Ok(mut children) = BRIDGE_CHILDREN.lock() {
        for mut child in children.drain(..) {
            applog::info("bridge", format!("killing bridge pid {}", child.id()));
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[tauri::command(async)]
async fn get_process_list_fast() -> Vec<ProcessInfo> {
    process_enumerator::enumerate_processes_fast()
}

#[tauri::command(async)]
async fn get_process_list() -> Vec<ProcessInfo> {
    process_enumerator::enumerate_processes_full()
}

#[tauri::command(async)]
async fn get_process_icon(pid: u32) -> Option<String> {
    process_enumerator::get_process_icon(pid)
}

#[tauri::command(async)]
async fn get_process_modules(pid: u32) -> Vec<ModuleInfo> {
    process_enumerator::enumerate_modules(pid)
}

#[tauri::command(async)]
async fn bridge64_health() -> bool {
    bridge_client::bridge64_health()
}

#[tauri::command(async)]
async fn bridge32_health() -> bool {
    bridge_client::bridge32_health()
}

#[tauri::command(async)]
async fn bridge_set_speed(factor: f64) -> bool {
    let a = bridge_client::bridge64_set_speed(factor);
    let b = bridge_client::bridge32_set_speed(factor);
    applog::info("process-control", format!(
        "SETSPEED {factor}: x64 {} / x86 {}",
        if a { "OK" } else { "FAILED" }, if b { "OK" } else { "FAILED" }
    ));
    a || b
}

#[tauri::command(async)]
async fn bridge_get_speed() -> Option<f64> {
    bridge_client::bridge64_get_speed()
}

#[tauri::command(async)]
async fn get_system_stats() -> system_stats::SystemStats {
    system_stats::get_system_stats()
}

/// PIDs this session has enabled, with their arch, so they can be switched
/// back to normal speed when OpenSpeedy exits. Upstream only killed the bridges
/// on exit, leaving the game running at the last speed with no UI to stop it.
static ENABLED_PIDS: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());

fn track_enabled(pid: u32, arch: &str, on: bool) {
    let mut v = match ENABLED_PIDS.lock() { Ok(g) => g, Err(p) => p.into_inner() };
    v.retain(|(p, _)| *p != pid);
    if on { v.push((pid, arch.to_string())); }
}

fn disable_all_enabled() {
    let list: Vec<(u32, String)> = match ENABLED_PIDS.lock() {
        Ok(mut g) => g.drain(..).collect(),
        Err(p) => p.into_inner().drain(..).collect(),
    };
    for (pid, arch) in list {
        let ok = if arch == "x86" { bridge_client::bridge32_disable(pid) } else { bridge_client::bridge64_disable(pid) };
        applog::info("process-control", format!("exit: DISABLE {pid} [{arch}]: {}", if ok { "OK" } else { "FAILED (process gone?)" }));
    }
}

/// Inject + enable. Returns true only when BOTH steps succeeded — upstream
/// returned the inject result alone and ignored ENABLE, and the UI ignored even
/// that, so a failed attempt still showed as "on" and was never retried.
#[tauri::command(async)]
async fn bridge_inject(pid: u32, arch: String) -> bool {
    let (inj, en) = if arch == "x86" {
        let inj = bridge_client::bridge32_inject(pid);
        (inj, inj && bridge_client::bridge32_enable(pid))
    } else {
        let inj = bridge_client::bridge64_inject(pid);
        (inj, inj && bridge_client::bridge64_enable(pid))
    };
    applog::info("process-control", format!("INJECT {pid} [{arch}]: {}", if inj { "OK" } else { "FAILED" }));
    if inj {
        applog::info("process-control", format!("ENABLE {pid} [{arch}]: {}", if en { "OK" } else { "FAILED" }));
    }
    track_enabled(pid, &arch, en);
    en
}

#[tauri::command(async)]
async fn bridge_enable(pid: u32, arch: String) -> bool {
    let ok = if arch == "x86" {
        bridge_client::bridge32_enable(pid)
    } else {
        bridge_client::bridge64_enable(pid)
    };
    applog::info("process-control", format!("ENABLE {pid} [{arch}]: {}", if ok { "OK" } else { "FAILED" }));
    track_enabled(pid, &arch, ok);
    ok
}

#[tauri::command(async)]
async fn bridge_disable(pid: u32, arch: String) -> bool {
    let ok = if arch == "x86" {
        bridge_client::bridge32_disable(pid)
    } else {
        bridge_client::bridge64_disable(pid)
    };
    applog::info("process-control", format!("DISABLE {pid} [{arch}]: {}", if ok { "OK" } else { "FAILED" }));
    track_enabled(pid, &arch, false);
    ok
}

/// Query bridge for per-PID status.
/// Returns Some(true) = enabled, Some(false) = injected but disabled, None = not injected.
#[tauri::command(async)]
async fn bridge_get_status(pid: u32, arch: String) -> Option<bool> {
    if arch == "x86" {
        bridge_client::bridge32_get_status(pid)
    } else {
        bridge_client::bridge64_get_status(pid)
    }
}

#[tauri::command(async)]
async fn set_always_on_top(window: tauri::Window, on_top: bool) {
    let _ = window.set_always_on_top(on_top);
}

/// Path of the diagnostics log, so the UI can reveal it in Explorer.
#[tauri::command(async)]
async fn get_log_path() -> String {
    applog::log_path_string()
}

/// Sink for frontend errors (`window.onerror`, unhandled rejections, React
/// render errors) so they end up in the same file as the Rust-side diagnostics.
#[tauri::command(async)]
async fn report_frontend_error(source: String, message: String, stack: Option<String>) {
    match stack.filter(|s| !s.trim().is_empty()) {
        Some(stack) => applog::error("frontend", format!("{source}: {message}\n{stack}")),
        None => applog::error("frontend", format!("{source}: {message}")),
    }
}

/// Start the GUI. Returns `Err` instead of panicking so the caller can write
/// the failure to the log and show it to the user — a panic here would make the
/// process disappear with no console and no trace.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<(), String> {
    // Idempotent — own entry point is `main.rs`, this covers any other caller.
    applog::init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_autostart::Builder::default().build())
        .plugin(tauri_plugin_shell::init())
        .setup(|_app| {
            applog::info("setup", "tauri setup callback entered");
            ensure_bridges();
            // Register Ctrl+C handler so bridges are killed on console exit
            unsafe { let _ = SetConsoleCtrlHandler(Some(ctrlc_handler), true); }
            applog::info("setup", "startup complete");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_process_list,
            get_process_list_fast,
            get_process_icon,
            get_process_modules,
            bridge64_health,
            bridge32_health,
            bridge_set_speed,
            bridge_get_speed,
            get_system_stats,
            bridge_inject,
            bridge_enable,
            bridge_disable,
            bridge_get_status,
            set_always_on_top,
            get_log_path,
            report_frontend_error,
        ])
        .device_event_filter(tauri::DeviceEventFilter::Always)
        .build(tauri::generate_context!())
        .map_err(|e| format!("failed to build the application (window/WebView2): {e}"))?;

    applog::info("startup", "app built, entering event loop");

    app.run(|_app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            applog::info("shutdown", "exit requested, restoring normal speed in enabled games");
            disable_all_enabled();
            applog::info("shutdown", "stopping bridges");
            shutdown_bridges();
        }
    });

    applog::info("shutdown", "event loop finished");
    Ok(())
}
