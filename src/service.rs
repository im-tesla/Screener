//! Windows service and interactive-session launcher.
//!
//! UAC prompts, the lock screen, and the security screen live on the `Winlogon`
//! secure desktop, which only a LocalSystem process can open. The normal server
//! therefore runs as a worker inside the user's session with a duplicated SYSTEM
//! token. A LocalSystem service (session 0) watches the active console session
//! and starts or restarts that worker. The first launch asks for administrator
//! approval once, through the normal UAC consent prompt, to copy the executable
//! and install the service; UAC itself always stays enabled.
use crate::util::log;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    fs,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::{
    core::{w, BOOL, PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Security::*,
        System::{
            Com::CoTaskMemFree,
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            JobObjects::*,
            RemoteDesktop::*,
            Services::*,
            Threading::*,
        },
        UI::{
            Shell::{
                FOLDERID_Desktop, IsUserAnAdmin, SHGetKnownFolderPath, ShellExecuteExW,
                KNOWN_FOLDER_FLAG, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
            },
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
};

const SERVICE_NAME: &str = "Screener";
const SERVICE_DISPLAY: &str = "Screener remote desktop";
const STILL_ACTIVE: u32 = 259;
/// `DELETE` access right, needed to remove a service.
const DELETE_ACCESS: u32 = 0x0001_0000;
static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct WorkerConfig {
    /// Hash of the executable the launcher wanted installed.
    source_hash: String,
    /// Server options, excluding service control flags.
    args: Vec<String>,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn install_directory() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Program Files"))
        .join("Screener")
}

fn installed_exe() -> PathBuf {
    install_directory().join("server.exe")
}

fn config_path() -> PathBuf {
    install_directory().join("worker.json")
}

fn read_config() -> Option<WorkerConfig> {
    serde_json::from_slice(&fs::read(config_path()).ok()?).ok()
}

fn write_config(config: &WorkerConfig) -> Result<()> {
    let path = config_path();
    fs::create_dir_all(path.parent().context("configuration directory")?)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(config)?)?;
    fs::rename(&temp, &path).context("writing the worker configuration")?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin().as_bool() }
}

fn is_system() -> bool {
    unsafe {
        let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut size = sid.len() as u32;
        let sid = PSID(sid.as_mut_ptr() as *mut c_void);
        if CreateWellKnownSid(WinLocalSystemSid, None, Some(sid), &mut size).is_err() {
            return false;
        }
        let mut member = BOOL(0);
        CheckTokenMembership(None, sid, &mut member).is_ok() && member.as_bool()
    }
}

/// Log which identity the worker runs under, so server.log shows whether UAC
/// prompts can be handled on this host.
pub fn describe_worker(session: Option<u32>) {
    let mut line = format!(
        "Session worker: session={}, desktop={}",
        session
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".into()),
        crate::desktop::current()
    );
    if is_system() {
        line.push_str(
            "; LocalSystem: the secure desktop (UAC prompts) can be captured and controlled",
        );
    } else {
        line.push_str(
            "; not LocalSystem: UAC prompts on the secure desktop cannot be captured or answered",
        );
    }
    log(line);
}

/// Strip flags that select service and worker behavior; everything else is a
/// normal server option that the service will pass to the session worker.
pub fn server_args(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip_value = false;
    for arg in raw {
        if skip_value {
            skip_value = false;
            continue;
        }
        match arg.as_str() {
            "--service" | "--worker" | "--install-service" | "--uninstall-service" => {}
            "--session" | "--desktop" => skip_value = true,
            _ => out.push(arg.clone()),
        }
    }
    out
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\r', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(ch);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

fn command_line(exe: &Path, args: &[String]) -> String {
    let mut line = format!("\"{}\"", exe.display());
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    line
}

/// Ask Windows for administrator approval through the normal UAC prompt and wait
/// for the elevated process. UAC is not bypassed or disabled.
fn elevate(args: &[String]) -> Result<u32> {
    let exe = std::env::current_exe()?;
    let file = wide(&exe.to_string_lossy());
    let parameters = wide(
        &args
            .iter()
            .map(|a| quote_arg(a))
            .collect::<Vec<_>>()
            .join(" "),
    );
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).context("requesting administrator approval")?;
        ensure!(!info.hProcess.is_invalid(), "administrator approval failed");
        let process = OwnedHandle::from_raw_handle(info.hProcess.0);
        let waited = WaitForSingleObject(HANDLE(process.as_raw_handle()), 300_000);
        ensure!(
            waited == WAIT_OBJECT_0,
            "the elevated process did not finish"
        );
        let mut code = 1u32;
        GetExitCodeProcess(HANDLE(process.as_raw_handle()), &mut code)
            .context("reading the elevated process exit code")?;
        Ok(code)
    }
}

/// Re-run this executable elevated with the given arguments and wait for it.
pub fn request_elevation(args: &[String]) -> Result<()> {
    let code = elevate(args)?;
    ensure!(
        code == 0,
        "the elevated operation did not complete (exit code {code}); see server.log"
    );
    Ok(())
}

unsafe fn service_state() -> Result<Option<SERVICE_STATUS_CURRENT_STATE>> {
    let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
        .context("opening the service manager")?;
    let name = wide(SERVICE_NAME);
    let service = OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_QUERY_STATUS);
    let _ = CloseServiceHandle(scm);
    let service = match service {
        Ok(service) => service,
        Err(_) => return Ok(None),
    };
    let mut status = SERVICE_STATUS_PROCESS::default();
    let bytes = std::slice::from_raw_parts_mut(
        &mut status as *mut _ as *mut u8,
        std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
    );
    let mut needed = 0u32;
    let result = QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed);
    let _ = CloseServiceHandle(service);
    result?;
    Ok(Some(status.dwCurrentState))
}

unsafe fn query_service(access: u32) -> Result<(SC_HANDLE, SC_HANDLE)> {
    let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
        .context("opening the service manager")?;
    let name = wide(SERVICE_NAME);
    match OpenServiceW(scm, PCWSTR(name.as_ptr()), access) {
        Ok(service) => Ok((scm, service)),
        Err(e) => {
            let _ = CloseServiceHandle(scm);
            Err(e).context("opening the Screener service")
        }
    }
}

unsafe fn wait_for_state(
    service: SC_HANDLE,
    target: SERVICE_STATUS_CURRENT_STATE,
    timeout: Duration,
) -> Result<SERVICE_STATUS_PROCESS> {
    let deadline = Instant::now() + timeout;
    let mut status = SERVICE_STATUS_PROCESS::default();
    loop {
        let bytes = std::slice::from_raw_parts_mut(
            &mut status as *mut _ as *mut u8,
            std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
        );
        let mut needed = 0u32;
        QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed)?;
        if status.dwCurrentState == target {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!(
                "the Screener service did not reach the expected state in time (state {})",
                status.dwCurrentState.0
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

unsafe fn start_installed_service() -> Result<()> {
    let (scm, service) = query_service(SERVICE_START | SERVICE_QUERY_STATUS)?;
    let started = StartServiceW(service, None);
    let result = wait_for_state(service, SERVICE_RUNNING, Duration::from_secs(15));
    let _ = CloseServiceHandle(service);
    let _ = CloseServiceHandle(scm);
    match result {
        Ok(_) => Ok(()),
        Err(e) => match started {
            Ok(()) => Err(e),
            Err(start) => Err(e.context(start)),
        },
    }
}

unsafe fn stop_installed_service() -> Result<()> {
    let (scm, service) = query_service(SERVICE_STOP | SERVICE_QUERY_STATUS)?;
    let mut status = SERVICE_STATUS::default();
    let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut status);
    let result = wait_for_state(service, SERVICE_STOPPED, Duration::from_secs(40));
    let _ = CloseServiceHandle(service);
    let _ = CloseServiceHandle(scm);
    result.map(|_| ())
}

/// Add Private-profile, local-subnet inbound rules for the installed executable.
/// Reinstalls replace the rules by name so the list stays idempotent.
fn configure_firewall(exe: &Path) -> Result<()> {
    let program = exe.to_string_lossy();
    let rules = [
        ("Screener LAN video (UDP) and control", "udp", "47990"),
        ("Screener LAN video and input", "tcp", "47990,47991,47993"),
        ("Screener LAN discovery", "udp", "47992"),
    ];
    let mut failures = 0;
    for (name, protocol, ports) in rules {
        let _ = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={name}"),
            ])
            .creation_flags(0x08000000)
            .output();
        let output = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={name}"),
                "dir=in",
                "action=allow",
                &format!("program={program}"),
                &format!("protocol={protocol}"),
                &format!("localport={ports}"),
                "profile=private",
                "remoteip=localsubnet",
                "enable=yes",
            ])
            .creation_flags(0x08000000)
            .output();
        match output {
            Ok(out) if out.status.success() => log(format!("Firewall rule added: {name}")),
            Ok(out) => {
                failures += 1;
                let detail = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout).trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                log(format!("Firewall rule '{name}' was not added: {detail}"));
            }
            Err(e) => {
                failures += 1;
                log(format!("Firewall rule '{name}' was not added: {e}"));
            }
        }
    }
    ensure!(
        failures == 0,
        "{failures} firewall rule(s) could not be added"
    );
    Ok(())
}

fn install_service(exe: &Path) -> Result<()> {
    let binary = wide(&format!("\"{}\" --service", exe.display()));
    let name = wide(SERVICE_NAME);
    let display = wide(SERVICE_DISPLAY);
    let account = wide("LocalSystem");
    unsafe {
        let scm = OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        )
        .context("opening the service manager")?;
        let existing = OpenServiceW(scm, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS);
        let service = match existing {
            Ok(service) => {
                ChangeServiceConfigW(
                    service,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(binary.as_ptr()),
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR(account.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR(display.as_ptr()),
                )
                .context("updating the Screener service")?;
                service
            }
            Err(_) => CreateServiceW(
                scm,
                PCWSTR(name.as_ptr()),
                PCWSTR(display.as_ptr()),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(binary.as_ptr()),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR(account.as_ptr()),
                PCWSTR::null(),
            )
            .context("creating the Screener service")?,
        };
        let mut description = wide(
            "Screener LAN remote desktop. Runs as LocalSystem in the interactive session so Windows UAC prompts and the sign-in screen can be controlled remotely. UAC remains enabled.",
        );
        let info = SERVICE_DESCRIPTIONW {
            lpDescription: PWSTR(description.as_mut_ptr()),
        };
        let _ = ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            Some(&info as *const _ as *const c_void),
        );
        // Restart the service automatically if it ever crashes, so remote access
        // does not disappear until the next reboot.
        let mut actions = [
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 5000,
            },
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 15000,
            },
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 60000,
            },
        ];
        let failure = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 3600,
            lpRebootMsg: PWSTR::null(),
            lpCommand: PWSTR::null(),
            cActions: actions.len() as u32,
            lpsaActions: actions.as_mut_ptr(),
        };
        let _ = ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&failure as *const _ as *const c_void),
        );
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
    }
    Ok(())
}

/// Copy the launcher into a protected directory so a local user cannot replace
/// the executable that the SYSTEM service runs. Retries while a previous worker
/// is still exiting and holding the image.
fn install_executable(source: &Path) -> Result<PathBuf> {
    let target = installed_exe();
    fs::create_dir_all(install_directory())?;
    let same = source
        .canonicalize()
        .ok()
        .zip(target.canonicalize().ok())
        .is_some_and(|(a, b)| a == b);
    if same {
        return Ok(target);
    }
    let mut last = None;
    for _ in 0..40 {
        match fs::copy(source, &target) {
            Ok(_) => return Ok(target),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    Err(last.unwrap()).context("copying server.exe into Program Files")
}

/// Elevated entry point: install or update the service, then start it.
pub fn install(args: &[String], firewall: bool) -> Result<()> {
    ensure!(
        is_elevated(),
        "administrator approval is required to install the Screener service"
    );
    log("Installing the Screener service (LocalSystem, automatic start)...");
    // Stop the old service before replacing its executable image.
    unsafe {
        if service_state()? == Some(SERVICE_RUNNING) {
            stop_installed_service().context("stopping the previous Screener service")?;
        }
    }
    let source = std::env::current_exe()?;
    let source_hash = hash_file(&source)?;
    let target = install_executable(&source)?;
    if firewall {
        if let Err(e) = configure_firewall(&target) {
            log(format!("Firewall configuration: {e:#}"));
            log("Remote viewers cannot connect until the ports are allowed; run allow-lan.ps1 from an elevated PowerShell.");
        }
    }
    write_config(&WorkerConfig {
        source_hash,
        args: args.to_vec(),
    })?;
    install_service(&target)?;
    unsafe { start_installed_service()? };
    log(format!(
        "Service installed from {}; the session worker starts as LocalSystem when a user is logged on.",
        target.display()
    ));
    Ok(())
}

fn state_name(state: SERVICE_STATUS_CURRENT_STATE) -> &'static str {
    for (value, name) in [
        (SERVICE_STOPPED, "stopped"),
        (SERVICE_START_PENDING, "start pending"),
        (SERVICE_STOP_PENDING, "stop pending"),
        (SERVICE_RUNNING, "running"),
        (SERVICE_CONTINUE_PENDING, "continue pending"),
        (SERVICE_PAUSE_PENDING, "pause pending"),
        (SERVICE_PAUSED, "paused"),
    ] {
        if state == value {
            return name;
        }
    }
    "unknown"
}

/// Diagnostic entry point: report the installed service without elevation.
pub fn report_status() -> Result<()> {
    match unsafe { service_state()? } {
        Some(state) => log(format!(
            "Screener service: {} (state {}){}",
            state_name(state),
            state.0,
            if is_elevated() { "" } else { ", not elevated" }
        )),
        None => log("Screener service: not installed"),
    }
    match read_config() {
        Some(config) => log(format!(
            "Installed for executable hash {}…; worker options: {:?}",
            &config.source_hash[..config.source_hash.len().min(12)],
            config.args
        )),
        None => log("No installed worker configuration (Program Files\\Screener\\worker.json)"),
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    ensure!(
        is_elevated(),
        "administrator approval is required to remove the Screener service"
    );
    unsafe {
        if let Ok(()) = stop_installed_service() {
            log("Screener service stopped");
        }
        if let Ok((scm, service)) = query_service(DELETE_ACCESS | SERVICE_QUERY_STATUS) {
            DeleteService(service).context("deleting the Screener service")?;
            let _ = CloseServiceHandle(service);
            let _ = CloseServiceHandle(scm);
            log("Screener service removed");
        }
    }
    let _ = fs::remove_file(config_path());
    // The running executable cannot delete itself; leave it when that happens.
    let _ = fs::remove_file(installed_exe());
    let _ = fs::remove_dir(install_directory());
    Ok(())
}

/// Unelevated entry point for `server.exe`: make sure the installed service is
/// current and running, asking for administrator approval only when needed.
pub fn ensure(args: &[String]) -> Result<()> {
    let source = std::env::current_exe()?;
    let hash = hash_file(&source)?;
    let config = read_config();
    let current = config
        .as_ref()
        .is_some_and(|c| c.source_hash == hash && c.args == args);
    match unsafe { service_state()? } {
        None => log("The Screener service is not installed; installing it now."),
        Some(_) if !current => {
            log("The installed Screener service is out of date for this executable or these options; updating it.")
        }
        Some(SERVICE_RUNNING) => {
            log("Screener is already running as a LocalSystem service; UAC prompts can be answered remotely.");
            return Ok(());
        }
        Some(state) => {
            log(format!(
                "The Screener service is installed but not running (state {}); starting it.",
                state.0
            ));
            match unsafe { start_installed_service() } {
                Ok(()) => {
                    log("Screener service started.");
                    return Ok(());
                }
                Err(e) => log(format!(
                    "Could not start the service without elevation ({e}); requesting administrator approval."
                )),
            }
        }
    }
    log("Windows will ask for administrator approval once to install the service. UAC stays enabled.");
    let mut elevated = vec!["--install-service".to_string()];
    elevated.extend(args.iter().cloned());
    request_elevation(&elevated)?;
    ensure!(
        unsafe { service_state()? } == Some(SERVICE_RUNNING),
        "the Screener service is installed but not running; see service.log"
    );
    log("Screener is running as a LocalSystem service in the interactive session.");
    Ok(())
}

/// Installer mode started by the service control manager.
pub fn run_service() -> Result<()> {
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::null(),
            lpServiceProc: None,
        },
    ];
    unsafe {
        StartServiceCtrlDispatcherW(table.as_ptr()).context(
            "starting the service dispatcher (--service is only valid when launched by the service manager)",
        )?;
    }
    Ok(())
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let handle = match RegisterServiceCtrlHandlerExW(w!("Screener"), Some(service_control), None) {
        Ok(handle) => handle,
        Err(e) => {
            log(format!("Service handler registration failed: {e}"));
            return;
        }
    };
    set_status(handle, SERVICE_START_PENDING, 0, 5000);
    set_status(
        handle,
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
        0,
    );
    supervise_sessions();
    set_status(handle, SERVICE_STOPPED, 0, 0);
}

fn set_status(
    handle: SERVICE_STATUS_HANDLE,
    state: SERVICE_STATUS_CURRENT_STATE,
    accepted: u32,
    hint: u32,
) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: hint,
    };
    unsafe {
        let _ = SetServiceStatus(handle, &status);
    }
}

unsafe extern "system" fn service_control(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        STOP.store(true, Ordering::Relaxed);
    }
    0
}

fn session_active(session: u32) -> bool {
    unsafe {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        if WTSQuerySessionInformationW(None, session, WTSConnectState, &mut buffer, &mut bytes)
            .is_err()
            || buffer.is_null()
            || bytes < std::mem::size_of::<i32>() as u32
        {
            return false;
        }
        let state = *(buffer.0 as *const i32);
        WTSFreeMemory(buffer.0 as *mut _);
        state == WTSActive.0
    }
}

fn active_session() -> Option<u32> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX {
        return None;
    }
    session_active(session).then_some(session)
}

struct Worker {
    process: OwnedHandle,
    _job: OwnedHandle,
    session: u32,
}

impl Worker {
    fn exited(&self) -> bool {
        let mut code = 0u32;
        let failed =
            unsafe { GetExitCodeProcess(HANDLE(self.process.as_raw_handle()), &mut code).is_err() };
        failed || code != STILL_ACTIVE
    }

    fn stop(&mut self) {
        unsafe {
            let _ = TerminateProcess(HANDLE(self.process.as_raw_handle()), 1);
            let _ = WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 5000);
        }
    }
}

fn supervise_sessions() {
    log("Screener service started; looking for the interactive console session.");
    let mut worker: Option<Worker> = None;
    let mut next_attempt = Instant::now();
    let mut logged_wait = false;
    while !STOP.load(Ordering::Relaxed) {
        if let Some(current) = worker.as_mut() {
            if current.exited() {
                log(format!(
                    "Session worker in session {} exited; restarting",
                    current.session
                ));
                worker = None;
                next_attempt = Instant::now() + Duration::from_secs(2);
            } else if Some(current.session) != active_session() {
                log("The interactive session changed; stopping the old worker.");
                current.stop();
                worker = None;
                next_attempt = Instant::now() + Duration::from_secs(1);
            }
        }
        if worker.is_none() && Instant::now() >= next_attempt {
            next_attempt = Instant::now() + Duration::from_secs(2);
            match active_session() {
                Some(session) => match launch_worker(session) {
                    Ok(created) => {
                        log(format!(
                            "Session worker started as LocalSystem in session {session} on winsta0\\default"
                        ));
                        worker = Some(created);
                        logged_wait = false;
                    }
                    Err(e) => log(format!("Session worker launch failed: {e:#}")),
                },
                None => {
                    if !logged_wait {
                        log("Waiting for an active console session with a signed-in user.");
                        logged_wait = true;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if let Some(mut current) = worker {
        current.stop();
    }
    log("Screener service stopped.");
}

unsafe fn enable_privileges() -> Result<()> {
    let mut token = HANDLE::default();
    OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
        &mut token,
    )?;
    let token = OwnedHandle::from_raw_handle(token.0);
    for name in [
        w!("SeTcbPrivilege"),
        w!("SeAssignPrimaryTokenPrivilege"),
        w!("SeIncreaseQuotaPrivilege"),
    ] {
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid).is_err() {
            continue;
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let _ = AdjustTokenPrivileges(
            HANDLE(token.as_raw_handle()),
            false,
            Some(&privileges),
            0,
            None,
            None,
        );
    }
    Ok(())
}

fn desktop_for_token(token: HANDLE) -> Option<PathBuf> {
    unsafe {
        let path =
            SHGetKnownFolderPath(&FOLDERID_Desktop, KNOWN_FOLDER_FLAG(0), Some(token)).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

fn launch_worker(session: u32) -> Result<Worker> {
    let config = read_config().unwrap_or_default();
    let exe = std::env::current_exe()?;
    let mut args = vec![
        "--worker".to_string(),
        "--session".to_string(),
        session.to_string(),
    ];
    args.extend(
        config
            .args
            .iter()
            .filter(|arg| {
                !matches!(
                    arg.as_str(),
                    "--service"
                        | "--worker"
                        | "--install-service"
                        | "--uninstall-service"
                        | "--no-firewall"
                )
            })
            .cloned(),
    );
    unsafe {
        enable_privileges()?;
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ACCESS_MASK(
                TOKEN_DUPLICATE.0
                    | TOKEN_QUERY.0
                    | TOKEN_ASSIGN_PRIMARY.0
                    | TOKEN_ADJUST_DEFAULT.0
                    | TOKEN_ADJUST_SESSIONID.0,
            ),
            &mut token,
        )
        .context("opening the service token")?;
        let token = OwnedHandle::from_raw_handle(token.0);
        let mut primary = HANDLE::default();
        DuplicateTokenEx(
            HANDLE(token.as_raw_handle()),
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
        .context("duplicating the LocalSystem token")?;
        let primary = OwnedHandle::from_raw_handle(primary.0);
        let id = session;
        SetTokenInformation(
            HANDLE(primary.as_raw_handle()),
            TokenSessionId,
            &id as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .context("moving the worker token into the interactive session")?;
        // The signed-in user's Desktop receives dropped files.
        let mut user = HANDLE::default();
        if WTSQueryUserToken(session, &mut user).is_ok() {
            let user = OwnedHandle::from_raw_handle(user.0);
            match desktop_for_token(HANDLE(user.as_raw_handle())) {
                Some(desktop) => {
                    args.push("--desktop".to_string());
                    args.push(desktop.to_string_lossy().into_owned());
                    log(format!("File drops target {}", desktop.display()));
                }
                None => log("Could not resolve the interactive user's Desktop; file drops use the service profile."),
            }
        }
        let mut environment = std::ptr::null_mut();
        let environment_created =
            CreateEnvironmentBlock(&mut environment, None, false).is_ok() && !environment.is_null();
        let mut command = wide(&command_line(&exe, &args));
        let mut desktop = wide("winsta0\\default");
        let cwd = exe.parent().unwrap_or(Path::new("C:\\"));
        let cwd = wide(&cwd.to_string_lossy());
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        let created = CreateProcessAsUserW(
            Some(HANDLE(primary.as_raw_handle())),
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            environment_created.then_some(environment),
            PCWSTR(cwd.as_ptr()),
            &startup,
            &mut info,
        );
        if environment_created {
            let _ = DestroyEnvironmentBlock(environment);
        }
        created.context("starting the interactive session worker")?;
        let _ = CloseHandle(info.hThread);
        let process = OwnedHandle::from_raw_handle(info.hProcess.0);
        let job = CreateJobObjectW(None, None).context("creating the worker job object")?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of_val(&limits) as u32,
        )
        .context("configuring the worker job object")?;
        AssignProcessToJobObject(job, HANDLE(process.as_raw_handle()))
            .context("assigning the worker to its job object")?;
        Ok(Worker {
            process,
            _job: OwnedHandle::from_raw_handle(job.0),
            session,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(quote_arg("--fps"), "--fps");
        assert_eq!(
            quote_arg("C:\\Documents\\report.pdf"),
            "C:\\Documents\\report.pdf"
        );
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn service_flags_are_not_forwarded_to_the_worker() {
        // --no-firewall is kept for the installer but filtered by the worker.
        let raw: Vec<String> = [
            "--service",
            "--width",
            "1280",
            "--session",
            "1",
            "--worker",
            "--no-firewall",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(server_args(&raw), vec!["--width", "1280", "--no-firewall"]);
    }
}
