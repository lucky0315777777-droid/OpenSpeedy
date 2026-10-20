//! OpenSpeedy Bridge — named-pipe server (Rust).
//!
//! Receives text commands from the main OpenSpeedy process:
//!   INJECT <pid>  EJECT <pid>  ENABLE <pid>  DISABLE <pid>
//!   ISENABLED <pid>  SETSPEED <factor>  GETSPEED  SHUTDOWN
//!
//!   STATUS <pid>  — "OK ENABLED" | "OK DISABLED" | "OK NOTINJECTED"
//!
//! Responses:  OK [value]  or  ERROR <message>

#![windows_subsystem = "windows"]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::core::{PCSTR, PCWSTR, PWSTR, s};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, BOOL, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{WriteFile, ReadFile, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetCurrentProcess, OpenProcess, QueryFullProcessImageNameW,
    WaitForSingleObject, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE, PROCESS_NAME_WIN32,
};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx,
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W,
    TH32CS_SNAPMODULE,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryW,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT, NAMED_PIPE_MODE,
};

// ── Crash protection ─────────────────────────────────────────────────────
// When calling into the speedpatch DLL, a target process that has exited
// can trigger an access violation. Rust's catch_unwind only catches Rust
// panics, not native crashes. We use:
// 1. VEH (AddVectoredExceptionHandler) — logs the crash
// 2. UEF (SetUnhandledExceptionFilter) — returns EXCEPTION_EXECUTE_HANDLER
//    so Windows terminates only the offending THREAD, not the PROCESS.
// Combined with spawn_protected (sacrificial thread), the bridge stays alive.

use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, SetUnhandledExceptionFilter,
    EXCEPTION_POINTERS, EXCEPTION_CONTINUE_SEARCH,
};

unsafe extern "system" fn bridge_veh_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    if let Some(record) = unsafe { info.as_ref() }.and_then(|i| unsafe { i.ExceptionRecord.as_ref() }) {
        eprintln!(
            "[bridge] native crash 0x{:08X} at 0x{:X} — VEH caught",
            record.ExceptionCode.0,
            record.ExceptionAddress as usize,
        );
    }
    EXCEPTION_CONTINUE_SEARCH
}

unsafe extern "system" fn bridge_uef_handler(info: *const EXCEPTION_POINTERS) -> i32 {
    if let Some(record) = unsafe { info.as_ref() }.and_then(|i| unsafe { i.ExceptionRecord.as_ref() }) {
        eprintln!(
            "[bridge] unhandled exception 0x{:08X} — terminating thread, bridge stays alive",
            record.ExceptionCode.0,
        );
    }
    1 // EXCEPTION_EXECUTE_HANDLER
}

fn install_crash_handler() {
    unsafe {
        AddVectoredExceptionHandler(1, Some(bridge_veh_handler));
        SetUnhandledExceptionFilter(Some(bridge_uef_handler));
    }
}

// ── Logging ──────────────────────────────────────────────────────────────
// stderr is redirected to bridgeXX.exe.log by the app. Every line now carries a
// local timestamp so it can be matched against openspeedy.YYYYMMDD.log.

fn log_ts() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

macro_rules! blog {
    ($($arg:tt)*) => {
        eprintln!("{} {}", crate::log_ts(), format!($($arg)*))
    };
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn exe_dir() -> PathBuf {
    let mut buf = vec![0u16; 260];
    let mut len = buf.len() as u32;
    let h = unsafe { GetCurrentProcess() };
    unsafe {
        let _ = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR::from_raw(buf.as_mut_ptr()), &mut len);
    }
    let s = String::from_utf16_lossy(&buf[..len as usize]);
    PathBuf::from(&s).parent().unwrap_or(&PathBuf::from(".")).to_path_buf()
}

fn is_process_64bit(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid) else {
            blog!("[bridge] is_process_64bit({pid}): OpenProcess failed, assuming 64-bit");
            return true;
        };
        let mut wow64 = BOOL::default();
        let wide_k32 = to_wide("kernel32.dll");
        let kernel32 = GetModuleHandleW(PCWSTR::from_raw(wide_k32.as_ptr())).unwrap_or_default();
        let is_wow64: Option<unsafe extern "system" fn(HANDLE, *mut BOOL) -> BOOL> =
            std::mem::transmute(GetProcAddress(kernel32, s!("IsWow64Process")));
        if let Some(f) = is_wow64 { f(h, &mut wow64); }
        let _ = CloseHandle(h);
        let result = !wow64.as_bool();
        blog!("[bridge] is_process_64bit({pid}): wow64={} -> is64={result}", wow64.as_bool());
        result
    }
}

#[cfg(target_arch = "x86_64")]
const OWN_SPEEDPATCH: &str = "speedpatch64.dll";
#[cfg(target_arch = "x86")]
const OWN_SPEEDPATCH: &str = "speedpatch32.dll";

fn speedpatch_dll(is64: bool) -> &'static str {
    if is64 { "speedpatch64.dll" } else { "speedpatch32.dll" }
}

// ── Own copy of the speedpatch DLL ───────────────────────────────────────
//
// The bridge itself loads speedpatch*.dll to call SP_SetSpeed / SP_Enable /
// ... (the speed factor lives in a section shared by every process that maps
// the DLL). Loading it runs DllMain, which hooks the *bridge's own* time APIs
// and — through SP_Install — marks the bridge as enabled.
//
// Upstream left it that way, so at speed ×10 the bridge's own
// `WaitForSingleObject(remote_thread, 5000)` during INJECT only waited 500 ms
// (×100 → 50 ms). When the game was busy (loading, holding the loader lock)
// the wait timed out, GetExitCodeThread returned STILL_ACTIVE (259 ≠ 0), that
// was counted as success, and the DLL path buffer was freed while the game's
// LoadLibraryW was still reading it. Result: "INJECT OK" in the log but no
// speed change — the intermittent failure.
//
// Fix: load the DLL once, by full path, and immediately disable speed control
// for the bridge's own PID so its hooks pass time through unchanged.

static OWN_DLL: std::sync::OnceLock<Option<isize>> = std::sync::OnceLock::new();

fn own_dll() -> Option<windows::Win32::Foundation::HMODULE> {
    use windows::Win32::Foundation::HMODULE;
    let raw = OWN_DLL.get_or_init(|| {
        let path = exe_dir().join(OWN_SPEEDPATCH);
        let wide = to_wide(&path.to_string_lossy());
        let h = match unsafe { LoadLibraryW(PCWSTR::from_raw(wide.as_ptr())) } {
            Ok(h) => h,
            Err(e) => {
                blog!("[bridge] cannot load own {}: {e:?}", path.display());
                return None;
            }
        };
        unsafe {
            let sp_disable: Option<unsafe extern "C" fn(u32)> =
                std::mem::transmute(GetProcAddress(h, s!("SP_Disable")));
            if let Some(f) = sp_disable {
                f(std::process::id());
                blog!("[bridge] loaded own {OWN_SPEEDPATCH}; speed control disabled for the bridge itself");
            }
        }
        Some(h.0 as isize)
    });
    raw.map(|v| HMODULE(v as *mut std::ffi::c_void))
}

/// Resolve an export of our own copy of the DLL.
fn own_proc(name: &'static [u8]) -> Option<unsafe extern "system" fn() -> isize> {
    let h = own_dll()?;
    unsafe { GetProcAddress(h, PCSTR::from_raw(name.as_ptr())) }
}

// ── Helpers on the target process ────────────────────────────────────────

/// Look up `dll_name` in the target's module list. Returns its base address.
fn find_module(pid: u32, dll_name: &str) -> Option<*mut std::ffi::c_void> {
    use windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPMODULE32;
    // SNAPMODULE32 is required to see modules of a 32-bit (WOW64) target from
    // a 64-bit snapshotting process; harmless otherwise.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) }.ok()?;
    let mut me = MODULEENTRY32W { dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32, ..Default::default() };
    let want = dll_name.to_lowercase();
    let mut found = None;
    unsafe {
        if Module32FirstW(snap, &mut me).is_ok() {
            loop {
                let name = String::from_utf16_lossy(&me.szModule).trim_end_matches('\0').to_lowercase();
                if name == want {
                    found = Some(me.hModule.0 as *mut std::ffi::c_void);
                    break;
                }
                if Module32NextW(snap, &mut me).is_err() { break; }
            }
        }
        let _ = CloseHandle(snap);
    }
    found
}

// ── Core operations ──────────────────────────────────────────────────────

const STILL_ACTIVE: u32 = 259;
/// How long to wait for the remote LoadLibrary. Generous on purpose: a game
/// that is loading assets can hold the loader lock for several seconds.
const INJECT_WAIT_MS: u32 = 15_000;

fn do_inject(pid: u32) -> Result<(), String> {
    if pid == 0 { return Err("invalid pid 0".into()); }
    let is64 = is_process_64bit(pid);
    let dll_name = speedpatch_dll(is64);
    blog!("[bridge] INJECT pid={pid} arch={} dll={dll_name}", if is64 { "x64" } else { "x86" });

    // Already there (e.g. OpenSpeedy was restarted while the game kept
    // running): nothing to load. LoadLibrary would only bump the refcount.
    if let Some(base) = find_module(pid, dll_name) {
        blog!("[bridge] INJECT pid={pid}: {dll_name} already loaded at {base:?} — skipping LoadLibrary");
        return Ok(());
    }

    let dll_path = exe_dir().join(dll_name);
    if !dll_path.exists() {
        return Err(format!("{} not found", dll_path.display()));
    }
    let dll_str = dll_path.to_string_lossy().to_string();

    let h_proc = unsafe {
        OpenProcess(
            PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION
            | PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ,
            false, pid,
        )
    }.map_err(|e| format!("OpenProcess: {e:?} (game running as admin while OpenSpeedy is not?)"))?;

    let mut result = try_inject_impl(&h_proc, &dll_str, "LoadLibraryW", true);
    if result.is_err() && find_module(pid, dll_name).is_none() {
        blog!("[bridge] LoadLibraryW path failed ({}), trying LoadLibraryA", result.as_ref().unwrap_err());
        result = try_inject_impl(&h_proc, &dll_str, "LoadLibraryA", false);
    }
    unsafe { let _ = CloseHandle(h_proc); }

    // The module list is the ground truth — the thread exit code is only the
    // low 32 bits of an HMODULE and says nothing if the wait timed out.
    match find_module(pid, dll_name) {
        Some(base) => {
            blog!("[bridge] INJECT pid={pid}: verified {dll_name} loaded at {base:?}");
            Ok(())
        }
        None => Err(match result {
            Err(e) => e,
            Ok(()) => format!("LoadLibrary reported success but {dll_name} is not in the target's module list"),
        }),
    }
}

/// Attempt injection with a specific LoadLibrary variant.
/// `wide` = true → use wide-char path (LoadLibraryW), false → ANSI (LoadLibraryA).
fn try_inject_impl(h_proc: &HANDLE, dll_path: &str, fn_name: &str, wide: bool) -> Result<(), String> {
    blog!("[bridge] try_inject_impl: dll={dll_path} fn={fn_name}");

    let path_bytes: Vec<u8> = if wide {
        dll_path.encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .chain([0u8, 0u8])
            .collect()
    } else {
        dll_path.as_bytes().iter()
            .copied()
            .chain([0u8])
            .collect()
    };
    let path_len = path_bytes.len();

    let remote_mem = unsafe {
        VirtualAllocEx(*h_proc, None, path_len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
    };
    if remote_mem.is_null() {
        return Err("VirtualAllocEx failed".into());
    }

    let wrote = unsafe {
        WriteProcessMemory(*h_proc, remote_mem, path_bytes.as_ptr() as _, path_len, None)
    };
    if let Err(e) = wrote {
        unsafe { let _ = VirtualFreeEx(*h_proc, remote_mem, 0, MEM_RELEASE); }
        return Err(format!("WriteProcessMemory: {e:?}"));
    }

    let wide_k32 = to_wide("kernel32.dll");
    let kernel32 = unsafe { GetModuleHandleW(PCWSTR::from_raw(wide_k32.as_ptr())) }
        .map_err(|e| format!("GetModuleHandleW(kernel32): {e:?}"))?;

    let fn_cstr = std::ffi::CString::new(fn_name).unwrap();
    let load_lib = unsafe { GetProcAddress(kernel32, PCSTR::from_raw(fn_cstr.as_ptr() as *const u8)) }
        .ok_or_else(|| format!("GetProcAddress({fn_name}) failed"))?;

    let h_thread = match unsafe {
        CreateRemoteThread(*h_proc, None, 0,
            Some(std::mem::transmute(load_lib)), Some(remote_mem), 0, None)
    } {
        Ok(h) => h,
        Err(e) => {
            unsafe { let _ = VirtualFreeEx(*h_proc, remote_mem, 0, MEM_RELEASE); }
            return Err(format!("CreateRemoteThread: {e:?}"));
        }
    };

    let started = std::time::Instant::now();
    let wait = unsafe { WaitForSingleObject(h_thread, INJECT_WAIT_MS) };
    let waited_ms = started.elapsed().as_millis();

    let mut exit_code = 0u32;
    let got_code = unsafe {
        use windows::Win32::System::Threading::GetExitCodeThread;
        GetExitCodeThread(h_thread, &mut exit_code)
    };
    unsafe { let _ = CloseHandle(h_thread); }
    blog!("[bridge] remote {fn_name}: wait={wait:?} after {waited_ms} ms, exit_code={exit_code:#x}");

    if got_code.is_err() || exit_code == STILL_ACTIVE {
        // The remote LoadLibrary is still running and still reading the path
        // buffer. Freeing it now is a use-after-free in the game. Leak the few
        // bytes instead; the caller checks the module list afterwards.
        blog!("[bridge] remote thread still running — path buffer intentionally not freed");
        return Err(format!("remote {fn_name} did not finish within {INJECT_WAIT_MS} ms"));
    }

    unsafe { let _ = VirtualFreeEx(*h_proc, remote_mem, 0, MEM_RELEASE); }

    if exit_code == 0 {
        Err(format!("remote {fn_name} returned NULL (DLL failed to load in the target)"))
    } else {
        Ok(())
    }
}

fn do_eject(pid: u32) -> Result<(), String> {
    let is64 = is_process_64bit(pid);
    let dll_name = speedpatch_dll(is64);
    let h_mod_ptr = find_module(pid, dll_name).ok_or("module not found in target")?;

    let h_proc = unsafe {
        OpenProcess(
            PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION,
            false, pid,
        )
    }.map_err(|e| format!("OpenProcess: {e:?}"))?;

    let wide_k32 = to_wide("kernel32.dll");
    let kernel32 = unsafe {
        GetModuleHandleW(PCWSTR::from_raw(wide_k32.as_ptr()))
    }.map_err(|_| "GetModuleHandleW failed")?;
    let free_lib = unsafe { GetProcAddress(kernel32, s!("FreeLibrary")) }
        .ok_or("GetProcAddress FreeLibrary failed")?;

    let h_thread = unsafe {
        CreateRemoteThread(h_proc, None, 0,
            Some(std::mem::transmute(free_lib)), Some(h_mod_ptr), 0, None)
    }.map_err(|e| format!("CreateRemoteThread: {e:?}"))?;

    unsafe { WaitForSingleObject(h_thread, INJECT_WAIT_MS); }
    unsafe { let _ = CloseHandle(h_thread); let _ = CloseHandle(h_proc); }
    Ok(())
}

fn sp_is_enabled_by_id(pid: u32) -> Option<bool> {
    let f = own_proc(b"SP_IsEnabledById\0")?;
    let f: unsafe extern "C" fn(u32) -> BOOL = unsafe { std::mem::transmute(f) };
    Some(unsafe { f(pid) }.as_bool())
}

fn do_enable(pid: u32) -> Result<(), String> {
    blog!("[bridge] ENABLE pid={pid}");
    let f = own_proc(b"SP_Enable\0").ok_or("SP_Enable not available (own DLL failed to load)")?;
    let sp_enable: unsafe extern "C" fn(u32) = unsafe { std::mem::transmute(f) };
    unsafe { sp_enable(pid) };

    // SP_Enable silently does nothing when the target has no speedpatch file
    // mapping (DLL not actually loaded there). Read the flag back so that case
    // becomes an error instead of a false "OK".
    match sp_is_enabled_by_id(pid) {
        Some(true) => {}
        Some(false) => return Err("target has no active speedpatch (DLL not loaded in the game?)".into()),
        None => blog!("[bridge] ENABLE pid={pid}: cannot read back status"),
    }

    // Shared failure counter — see speedpatch.h. It also counts failures of
    // other processes that loaded the DLL, so it is a hint, not a verdict.
    if let Some(f) = own_proc(b"SP_GetHookFailures\0") {
        let failures: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(f) };
        let n = unsafe { failures() };
        if n > 0 {
            blog!("[bridge] WARNING: {n} time-API hook(s) not installed (in some process) — those APIs stay unpatched");
        } else {
            blog!("[bridge] all time-API hooks installed");
        }
    }
    blog!("[bridge] ENABLE pid={pid} ok");
    Ok(())
}

fn do_disable(pid: u32) -> Result<(), String> {
    blog!("[bridge] DISABLE pid={pid}");
    let f = own_proc(b"SP_Disable\0").ok_or("SP_Disable not available (own DLL failed to load)")?;
    let sp_disable: unsafe extern "C" fn(u32) = unsafe { std::mem::transmute(f) };
    unsafe { sp_disable(pid) };
    Ok(())
}

fn do_is_enabled(_pid: u32) -> Result<bool, String> {
    // Kept for protocol compatibility; STATUS is the reliable query.
    Ok(true)
}

/// STATUS <pid> → "OK ENABLED" | "OK DISABLED" | "OK NOTINJECTED"
fn do_status(pid: u32) -> String {
    let is64 = is_process_64bit(pid);
    if find_module(pid, speedpatch_dll(is64)).is_none() {
        return "OK NOTINJECTED".into();
    }
    match sp_is_enabled_by_id(pid) {
        Some(true) => "OK ENABLED".into(),
        Some(false) => "OK DISABLED".into(),
        None => "ERROR status unavailable".into(),
    }
}

fn do_set_speed(factor: f64) -> Result<(), String> {
    if !(factor > 0.0) || !factor.is_finite() {
        return Err(format!("invalid speed factor {factor}"));
    }
    let f = own_proc(b"SP_SetSpeed\0").ok_or("SP_SetSpeed not available (own DLL failed to load)")?;
    let set_speed: unsafe extern "C" fn(f64) = unsafe { std::mem::transmute(f) };
    unsafe { set_speed(factor) };
    blog!("[bridge] SETSPEED {factor}");
    Ok(())
}

fn do_get_speed() -> f64 {
    match own_proc(b"SP_GetSpeed\0") {
        Some(f) => {
            let get_speed: unsafe extern "C" fn() -> f64 = unsafe { std::mem::transmute(f) };
            unsafe { get_speed() }
        }
        None => 1.0,
    }
}

// ── Command dispatch ─────────────────────────────────────────────────────

fn handle_command(line: &str) -> String {
    let mut parts = line.trim().split_whitespace();
    let cmd = parts.next().unwrap_or("").to_uppercase();

    match cmd.as_str() {
        "INJECT" => {
            let pid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            match do_inject(pid) { Ok(()) => "OK".into(), Err(e) => format!("ERROR {e}") }
        }
        "EJECT" => {
            let pid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            match do_eject(pid) { Ok(()) => "OK".into(), Err(e) => format!("ERROR {e}") }
        }
        "ENABLE" => {
            let pid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            match do_enable(pid) { Ok(()) => "OK".into(), Err(e) => format!("ERROR {e}") }
        }
        "DISABLE" => {
            let pid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            match do_disable(pid) { Ok(()) => "OK".into(), Err(e) => format!("ERROR {e}") }
        }
        "ISENABLED" => {
            let pid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            match do_is_enabled(pid) {
                Ok(true) => "OK 1".into(), Ok(false) => "OK 0".into(),
                Err(e) => format!("ERROR {e}"),
            }
        }
        "SETSPEED" => {
            let f: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
            match do_set_speed(f) { Ok(()) => "OK".into(), Err(e) => format!("ERROR {e}") }
        }
        "GETSPEED" => {
            let s = do_get_speed();
            format!("OK {s:.6}")
        }
        "STATUS" => {
            let pid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            do_status(pid)
        }
        "SHUTDOWN" => "OK shutting down".into(),
        _ => "ERROR unknown command".into(),
    }
}

fn write_resp(h_pipe: HANDLE, msg: &str) {
    let mut written = 0u32;
    unsafe { let _ = WriteFile(h_pipe, Some(msg.as_bytes()), Some(&mut written), None); }
}

// ── Pipe server ───────────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
const PIPE_NAME: &str = r"\\.\pipe\OpenSpeedyBridge64";
#[cfg(target_arch = "x86")]
const PIPE_NAME: &str = r"\\.\pipe\OpenSpeedyBridge32";

fn pipe_server() {
    loop {
        let name_wide = to_wide(PIPE_NAME);
        let h_pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR::from_raw(name_wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(3), // PIPE_ACCESS_DUPLEX
                NAMED_PIPE_MODE(PIPE_TYPE_MESSAGE.0 | PIPE_READMODE_MESSAGE.0 | PIPE_WAIT.0),
                1, 4096, 4096, 0, None,
            )
        };
        if h_pipe == INVALID_HANDLE_VALUE {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }

        let connected = unsafe { ConnectNamedPipe(h_pipe, None) };
        let connected = connected.is_ok() || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        if !connected {
            unsafe { let _ = CloseHandle(h_pipe); }
            continue;
        }

        let mut buf = [0u8; 4096];
        loop {
            let mut nread = 0u32;
            let ok = unsafe { ReadFile(h_pipe, Some(&mut buf), Some(&mut nread), None) };
            if ok.is_err() || nread == 0 { break; }

            let text = String::from_utf8_lossy(&buf[..nread as usize]);
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() { continue; }
                if line.eq_ignore_ascii_case("SHUTDOWN") {
                    write_resp(h_pipe, "OK shutting down\n");
                    unsafe { let _ = CloseHandle(h_pipe); }
                    std::process::exit(0);
                }
                let resp = handle_command(line);
                write_resp(h_pipe, &format!("{resp}\n"));
            }
        }
        unsafe { let _ = CloseHandle(h_pipe); }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────

fn main() {
    install_crash_handler();
    blog!("[bridge] ---- {} starting (pid {}) ----", PIPE_NAME, std::process::id());
    // Load our own copy of the DLL up front and switch speed control off for
    // the bridge process itself (see own_dll()).
    let _ = own_dll();

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let line = args[1..].join(" ");
        println!("{}", handle_command(&line));
        return;
    }
    pipe_server();
}
