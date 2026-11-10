//! Copies the release executable next to `Cargo.toml` after every build.
//!
//! A build script runs *before* the crate is compiled, so it cannot copy the
//! result itself. Instead it starts a short-lived background copy of itself that
//! waits for `target/release/okazuri-search-api[.exe]` to be (re)written and then
//! copies it to the project root. The background copy gives up after half an hour
//! and writes what it did to `target/release/copy-to-root.log`.
//!
//! Only release builds are copied, so a debug build never replaces the executable
//! in the project root. Set `OKAZURI_SEARCH_API_NO_COPY=1` to turn the copy off.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BIN: &str = "okazuri-search-api";
const WATCH_FLAG: &str = "--copy-when-built";
const GIVE_UP: Duration = Duration::from_secs(30 * 60);

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some(WATCH_FLAG) {
        watch(&args[2..]);
        return;
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=config.example.toml");
    println!("cargo:rerun-if-env-changed=OKAZURI_SEARCH_API_NO_COPY");

    if env::var("PROFILE").as_deref() != Ok("release") || env::var_os("OKAZURI_SEARCH_API_NO_COPY").is_some() {
        return;
    }
    let (Some(out_dir), Some(manifest_dir)) = (env::var_os("OUT_DIR"), env::var_os("CARGO_MANIFEST_DIR")) else {
        return;
    };
    // OUT_DIR is target/<profile>/build/<package>-<hash>/out
    let Some(profile_dir) = Path::new(&out_dir).ancestors().nth(3) else { return };
    let exe_name = format!("{}{}", BIN, env::consts::EXE_SUFFIX);
    let built = profile_dir.join(&exe_name);
    let destination = Path::new(&manifest_dir).join(&exe_name);
    let started = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    let Ok(this) = env::current_exe() else { return };
    let args = [
        WATCH_FLAG.to_string(),
        built.display().to_string(),
        destination.display().to_string(),
        started.to_string(),
    ];
    spawn_detached(&this, &args);
}

/// Starts `exe args` in the background, holding on to nothing of ours.
///
/// Windows hands a child *every* inheritable handle of its parent, and here that
/// includes the pipes cargo (and whatever started cargo: a terminal, an IDE, a CI
/// log) reads our output from. A child holding one keeps that reader waiting until
/// it exits, and the executable it waits for is only built after this script has
/// finished -- a deadlock. So on Windows the process is created directly, without
/// inheriting any handle.
#[cfg(windows)]
fn spawn_detached(exe: &Path, args: &[String]) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2_len: u16,
        reserved2: *mut u8,
        std_input: *mut c_void,
        std_output: *mut c_void,
        std_error: *mut c_void,
    }
    #[repr(C)]
    struct ProcessInformation {
        process: *mut c_void,
        thread: *mut c_void,
        process_id: u32,
        thread_id: u32,
    }
    extern "system" {
        fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *mut c_void,
            current_directory: *const u16,
            startup_info: *mut StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    // Cargo runs its children in a job that is killed when cargo exits; leave it
    // if the job allows that.
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().collect() };
    let mut line: Vec<u16> = Vec::new();
    line.push(b'"' as u16);
    line.extend(wide(exe.as_os_str()));
    line.push(b'"' as u16);
    for arg in args {
        line.push(b' ' as u16);
        line.push(b'"' as u16);
        line.extend(wide(std::ffi::OsStr::new(arg)));
        line.push(b'"' as u16);
    }
    line.push(0);

    for flags in [DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB, DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP] {
        let mut command_line = line.clone();
        // SAFETY: zeroed structs of plain integers and null pointers are valid for
        // these Win32 calls, and the buffers outlive them.
        unsafe {
            let mut startup: StartupInfoW = std::mem::zeroed();
            startup.cb = std::mem::size_of::<StartupInfoW>() as u32;
            let mut info: ProcessInformation = std::mem::zeroed();
            let ok = CreateProcessW(
                std::ptr::null(),
                command_line.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                flags,
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut startup,
                &mut info,
            );
            if ok != 0 {
                CloseHandle(info.process);
                CloseHandle(info.thread);
                return;
            }
        }
    }
}

#[cfg(not(windows))]
fn spawn_detached(exe: &Path, args: &[String]) {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let _ = cmd.spawn();
}

fn log(dir: &Path, message: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join("copy-to-root.log")) {
        let _ = writeln!(f, "{}", message);
    }
}

/// args: built executable, destination, unix time the build started.
fn watch(args: &[String]) {
    let [built, destination, started] = args else { return };
    let (built, destination) = (PathBuf::from(built), PathBuf::from(destination));
    let started: u64 = started.parse().unwrap_or(0);
    let log_dir = built.parent().map(Path::to_path_buf).unwrap_or_default();
    let began = std::time::Instant::now();

    let modified = |p: &Path| -> Option<(u64, u64)> {
        let meta = fs::metadata(p).ok()?;
        let secs = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
        Some((secs, meta.len()))
    };

    while began.elapsed() < GIVE_UP {
        sleep(Duration::from_millis(300));
        // Written by this build, and no longer growing.
        let Some(first) = modified(&built) else { continue };
        if first.0 + 2 < started || first.1 == 0 {
            continue;
        }
        sleep(Duration::from_millis(700));
        if modified(&built) != Some(first) {
            continue;
        }

        // The destination may be in use (a running copy cannot be replaced on
        // Windows), so retry for a while before giving up.
        let temp = destination.with_extension("tmp");
        let mut last_error = String::new();
        for _ in 0..60 {
            match fs::copy(&built, &temp).and_then(|_| fs::rename(&temp, &destination)) {
                Ok(()) => {
                    log(&log_dir, &format!("copied {} -> {}", built.display(), destination.display()));
                    return;
                }
                Err(e) => {
                    last_error = e.to_string();
                    let _ = fs::remove_file(&temp);
                    sleep(Duration::from_secs(1));
                }
            }
        }
        log(&log_dir, &format!("could not copy {} -> {}: {}", built.display(), destination.display(), last_error));
        return;
    }
    log(&log_dir, "gave up waiting for the build to finish");
}
