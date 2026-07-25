//! Windows named-pipe host for native render-tap adapters.
//!
//! The injected DLLs are pipe clients. This module owns no injection mechanism:
//! startup injection may be supplied by an administrator/service, while adapters
//! safely remain dormant and retry this per-logon-session pipe when shellglass is
//! absent.

#![cfg(windows)]

use crate::native_broker::{BrokerCommand, NativeBroker};
use crate::native_protocol::{Control, Decoder, Message, encode_control};
use crate::source::SourceSession;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    EqualSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    SECURITY_ATTRIBUTES, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenIntegrityLevel,
    TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, GetMessageW, MSG, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS,
};

/// A per-logon-session name. Client PID/token checks below are the authority;
/// the session suffix prevents unrelated interactive sessions contending for it.
static ACTIVE_RUNTIME: OnceLock<Mutex<Weak<Runtime>>> = OnceLock::new();
static DESIRED_PAUSED: AtomicBool = AtomicBool::new(false);

/// Create a local-only pipe whose DACL grants full access only to SYSTEM and
/// the broker's current user SID. Token verification after connect remains a
/// second boundary and additionally enforces equal integrity level.
fn create_user_pipe(name: &str, first: bool) -> Result<NamedPipeServer> {
    let token = process_token(unsafe { GetCurrentProcess() })?;
    let user = token_information(token.0, TokenUser)?;
    // SAFETY: TOKEN_USER buffer is aligned, initialized, and alive for this call.
    let sid = unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let mut sid_text = std::ptr::null_mut();
    // SAFETY: valid token SID and output pointer; API allocates with LocalAlloc.
    if unsafe { ConvertSidToStringSidW(sid, &mut sid_text) } == 0 {
        bail!("converting current user SID failed");
    }
    let sid_guard = LocalGuard(sid_text.cast());
    let mut length = 0usize;
    // SAFETY: allocated SID string is NUL-terminated.
    while unsafe { *sid_text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: length was found within the API-owned NUL-terminated string.
    let sid_string = String::from_utf16(unsafe { std::slice::from_raw_parts(sid_text, length) })?;
    // Low-integrity headless ConPTY conhosts are a normal Windows topology.
    // The DACL still names only this user; the low mandatory label lets that
    // same user's sandboxed conhost write upward. `verify_client` rejects the
    // dangerous opposite direction (a higher-integrity terminal into a lower
    // broker).
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid_string})S:(ML;;NW;;;LW)");
    let mut wide: Vec<u16> = sddl.encode_utf16().collect();
    wide.push(0);
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated SDDL and output pointer.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        bail!("building current-user pipe security descriptor failed");
    }
    let descriptor_guard = LocalGuard(descriptor);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_guard.0,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first);
    // SAFETY: attributes and its LocalAlloc-owned descriptor remain valid for
    // the complete synchronous CreateNamedPipeW call.
    let pipe = unsafe {
        options.create_with_security_attributes_raw(
            name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }?;
    drop(sid_guard);
    Ok(pipe)
}

struct LocalGuard(*mut core::ffi::c_void);

impl Drop for LocalGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: pointer came from a LocalAlloc-returning conversion API.
            unsafe { LocalFree(self.0) };
        }
    }
}

pub fn pipe_name() -> Result<String> {
    let mut session = 0u32;
    // SAFETY: valid output pointer; current PID always exists.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        bail!("querying the current Windows logon session failed");
    }
    Ok(format!(r"\\.\pipe\shellglass-render-tap-{session}"))
}

/// Start the native broker, named-pipe listener, and foreground tracker.
pub fn start(keep_last_terminal: bool) -> Result<SourceSession> {
    start_named(keep_last_terminal, pipe_name()?)
}

/// Start native terminal capture with accessibility reconstruction whenever the
/// foreground HWND has no matching native terminal source.
#[cfg(feature = "accessibility")]
pub fn start_hybrid(
    accessibility: crate::accessibility::AccessibilityOptions,
) -> Result<SourceSession> {
    let (broker, source) = start_named_broker(false, pipe_name()?)?;
    let wanted_broker = Arc::clone(&broker);
    let publish_broker = Arc::clone(&broker);
    crate::accessibility::spawn(
        accessibility,
        move || wanted_broker.wants_accessibility(),
        move |identity, frame| {
            publish_broker.publish_accessibility(identity, frame);
        },
    )?;
    Ok(source)
}

fn start_named(keep_last_terminal: bool, name: String) -> Result<SourceSession> {
    Ok(start_named_broker(keep_last_terminal, name)?.1)
}

fn start_named_broker(
    keep_last_terminal: bool,
    name: String,
) -> Result<(Arc<NativeBroker>, SourceSession)> {
    // FIRST_PIPE_INSTANCE makes a second broker fail loud instead of silently
    // sharing the adapter namespace with another shellglass process.
    let first = create_user_pipe(&name, true)
        .with_context(|| format!("creating native render-tap pipe {name}"))?;
    let (broker, source) = NativeBroker::new_with_policy(keep_last_terminal);
    let runtime = Arc::new(Runtime {
        broker: Arc::clone(&broker),
        routes: Mutex::new(HashMap::new()),
    });
    if DESIRED_PAUSED.load(Ordering::Acquire) {
        runtime.broker.set_paused(true);
    }
    *ACTIVE_RUNTIME
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .unwrap() = Arc::downgrade(&runtime);
    let accept_runtime = Arc::clone(&runtime);
    tokio::spawn(async move {
        if let Err(error) = accept_loop(first, name, accept_runtime).await {
            eprintln!("shellglass: native render-tap pipe stopped: {error:#}");
        }
    });
    runtime.dispatch(runtime.broker.foreground_changed(foreground_hwnd()));
    if !start_foreground_hook() {
        let foreground_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            foreground_poll_fallback(foreground_runtime).await;
        });
    }
    Ok((broker, source))
}

fn control_pipe_name() -> Result<String> {
    Ok(format!("{}-control", pipe_name()?))
}

/// Start the detached stream worker's local control plane. Creating the first
/// instance is synchronous, so `stream start` can reliably detect readiness.
pub fn start_control_server() -> Result<()> {
    let name = control_pipe_name()?;
    let first = create_user_pipe(&name, true)
        .with_context(|| format!("creating stream control pipe {name}"))?;
    tokio::spawn(async move {
        if let Err(error) = control_accept_loop(first, name).await {
            eprintln!("shellglass: stream control pipe stopped: {error:#}");
        }
    });
    Ok(())
}

/// Send one command to the detached stream worker.
pub async fn control(command: &str) -> Result<String> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = control_pipe_name()?;
    let mut pipe = ClientOptions::new()
        .open(&name)
        .with_context(|| "no detached shellglass stream is running")?;
    pipe.write_all(format!("{command}\n").as_bytes()).await?;
    pipe.flush().await?;
    let mut response = Vec::new();
    pipe.take(4_096).read_to_end(&mut response).await?;
    String::from_utf8(response).context("stream worker returned invalid UTF-8")
}

async fn control_accept_loop(mut next: NamedPipeServer, name: String) -> Result<()> {
    loop {
        next.connect()
            .await
            .context("accepting stream control client")?;
        let replacement =
            create_user_pipe(&name, false).context("creating next stream control pipe instance")?;
        let connected = std::mem::replace(&mut next, replacement);
        tokio::spawn(async move {
            if let Err(error) = control_connection(connected).await {
                eprintln!("shellglass: stream control request rejected: {error:#}");
            }
        });
    }
}

async fn control_connection(mut pipe: NamedPipeServer) -> Result<()> {
    verify_client(&pipe, false)?;
    let mut request = Vec::with_capacity(32);
    loop {
        let mut chunk = [0u8; 32];
        let count = tokio::time::timeout(Duration::from_secs(5), pipe.read(&mut chunk))
            .await
            .context("stream control command timed out")??;
        if count == 0 {
            bail!("unterminated stream control command");
        }
        request.extend_from_slice(&chunk[..count]);
        if request.contains(&b'\n') {
            break;
        }
        if request.len() >= 128 {
            bail!("stream control command exceeds limit");
        }
    }
    let newline = request.iter().position(|byte| *byte == b'\n').unwrap();
    if request[newline + 1..]
        .iter()
        .any(|byte| !byte.is_ascii_whitespace())
    {
        bail!("multiple stream control commands on one connection");
    }
    let request = std::str::from_utf8(&request[..newline])?.trim();
    let runtime = ACTIVE_RUNTIME
        .get()
        .and_then(|slot| slot.lock().unwrap().upgrade());
    let response = match request {
        "pause" => {
            DESIRED_PAUSED.store(true, Ordering::Release);
            if let Some(runtime) = &runtime {
                runtime.dispatch(runtime.broker.set_paused(true));
            }
            "paused\n".to_string()
        }
        "resume" => {
            DESIRED_PAUSED.store(false, Ordering::Release);
            if let Some(runtime) = &runtime {
                runtime.dispatch(runtime.broker.set_paused(false));
            }
            "streaming\n".to_string()
        }
        "status" => match runtime {
            Some(runtime) => {
                let (paused, sources, selected, accessibility) = runtime.broker.status();
                format!(
                    "{}; sources={sources}; selected={}\n",
                    if paused { "paused" } else { "streaming" },
                    selected
                        .map(|key| format!("terminal:{}:{}", key.process_nonce, key.source_id))
                        .or_else(|| accessibility.then(|| "accessibility".to_string()))
                        .unwrap_or_else(|| "none".to_string())
                )
            }
            None => format!(
                "{}; sources=0; selected=none\n",
                if DESIRED_PAUSED.load(Ordering::Acquire) {
                    "paused"
                } else {
                    "starting"
                }
            ),
        },
        "stop" => "stopping\n".to_string(),
        _ => bail!("unknown stream control command"),
    };
    pipe.write_all(response.as_bytes()).await?;
    pipe.flush().await?;
    if request == "stop" {
        // The detached worker owns no terminal or child command. Process exit
        // closes adapter pipes (making engines dormant) and the hub WebSocket.
        std::process::exit(0);
    }
    Ok(())
}

struct Runtime {
    broker: Arc<NativeBroker>,
    routes: Mutex<HashMap<u64, mpsc::UnboundedSender<BrokerCommand>>>,
}

impl Runtime {
    fn dispatch(&self, commands: Vec<BrokerCommand>) {
        let routes = self.routes.lock().unwrap();
        for command in commands {
            let nonce = match command {
                BrokerCommand::Subscribe { key, .. }
                | BrokerCommand::Unsubscribe { key, .. }
                | BrokerCommand::RequestFull { key, .. } => key.process_nonce,
            };
            if let Some(route) = routes.get(&nonce) {
                let _ = route.send(command);
            }
        }
    }
}

async fn accept_loop(mut next: NamedPipeServer, name: String, runtime: Arc<Runtime>) -> Result<()> {
    loop {
        next.connect()
            .await
            .context("accepting render-tap adapter")?;
        // Create the next instance before handing this one away, minimizing the
        // interval in which a newly injected adapter cannot connect.
        let replacement =
            create_user_pipe(&name, false).context("creating the next render-tap pipe instance")?;
        let connected = std::mem::replace(&mut next, replacement);
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            if let Err(error) = connection(connected, Arc::clone(&runtime)).await {
                eprintln!("shellglass: native adapter disconnected: {error:#}");
            }
        });
    }
}

async fn connection(pipe: NamedPipeServer, runtime: Arc<Runtime>) -> Result<()> {
    verify_client(&pipe, true).context("rejecting render-tap adapter identity")?;
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<BrokerCommand>();
    let mut decoder = Decoder::default();
    let mut nonce = None;
    let mut write_sequence = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    let hello_deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(hello_deadline);

    let result: Result<()> = async {
        loop {
            tokio::select! {
                () = &mut hello_deadline, if nonce.is_none() => {
                    bail!("native adapter did not send HELLO within five seconds");
                }
                read = reader.read(&mut buffer) => {
                    let count = read.context("reading native adapter pipe")?;
                    if count == 0 {
                        break;
                    }
                    for packet in decoder.push(&buffer[..count])? {
                        if nonce.is_none() {
                            if !matches!(packet.message, Message::Hello(_)) {
                                bail!("adapter did not introduce itself");
                            }
                            let process_nonce = packet.process_nonce;
                            let mut routes = runtime.routes.lock().unwrap();
                            if routes.contains_key(&process_nonce) {
                                bail!("adapter process nonce is already connected");
                            }
                            routes.insert(process_nonce, command_tx.clone());
                            nonce = Some(process_nonce);
                        }
                        runtime.dispatch(runtime.broker.handle(packet)?);
                    }
                }
                command = command_rx.recv() => {
                    let Some(command) = command else { break };
                    let Some(process_nonce) = nonce else { continue };
                    let control = broker_control(command);
                    write_sequence = write_sequence.checked_add(1).context("native control sequence exhausted")?;
                    writer.write_all(&encode_control(control, process_nonce, write_sequence)).await
                        .context("writing native adapter command")?;
                    writer.flush().await.context("flushing native adapter command")?;
                }
            }
        }
        Ok(())
    }
    .await;

    // Always tear down registry state, including malformed-message/write-error
    // exits through `?`; otherwise a dead source could remain selected forever.
    if let Some(process_nonce) = nonce {
        runtime.routes.lock().unwrap().remove(&process_nonce);
        runtime.dispatch(runtime.broker.disconnected(process_nonce));
    }
    result
}

fn broker_control(command: BrokerCommand) -> Control {
    match command {
        BrokerCommand::Subscribe {
            key,
            generation,
            max_fps,
        } => Control::Subscribe {
            source_id: key.source_id,
            generation,
            max_fps,
        },
        BrokerCommand::Unsubscribe { key, generation } => Control::Unsubscribe {
            source_id: key.source_id,
            generation,
        },
        BrokerCommand::RequestFull { key, generation } => Control::RequestFull {
            source_id: key.source_id,
            generation,
        },
    }
}

fn foreground_hwnd() -> Option<u64> {
    // SAFETY: GetForegroundWindow has no arguments and returns null when no
    // window is active. HWND values are opaque identities, never dereferenced.
    let hwnd = unsafe { GetForegroundWindow() } as usize as u64;
    (hwnd != 0).then_some(hwnd)
}

unsafe extern "system" fn foreground_callback(
    _hook: *mut core::ffi::c_void,
    event: u32,
    hwnd: *mut core::ffi::c_void,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND {
        return;
    }
    let current = (hwnd as usize != 0).then_some(hwnd as usize as u64);
    if let Some(runtime) = ACTIVE_RUNTIME
        .get()
        .and_then(|slot| slot.lock().unwrap().upgrade())
    {
        runtime.dispatch(runtime.broker.foreground_changed(current));
    }
}

/// Install the design's out-of-context foreground WinEvent hook on a dedicated
/// message-loop thread. Return false only when setup fails; callers then use the
/// cheap polling fallback rather than losing source selection entirely.
fn start_foreground_hook() -> bool {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        // SAFETY: callback has WINEVENTPROC's exact ABI; null module plus
        // OUTOFCONTEXT is the documented cross-process form.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                std::ptr::null_mut(),
                Some(foreground_callback),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        let installed = !hook.is_null();
        let _ = ready_tx.send(installed);
        if !installed {
            return;
        }
        let mut message: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: this thread owns the hook and pumps its ordinary Win32 message
        // queue until shutdown/error. Process teardown reclaims it if no WM_QUIT.
        while unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } > 0 {}
        unsafe { UnhookWinEvent(hook) };
    });
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or(false)
}

async fn foreground_poll_fallback(runtime: Arc<Runtime>) {
    let mut previous = foreground_hwnd();
    loop {
        let current = foreground_hwnd();
        if current != previous {
            previous = current;
            runtime.dispatch(runtime.broker.foreground_changed(current));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Reject cross-user clients and clients above the broker's integrity before
/// parsing a byte. Adapter pipes may accept same-user low-integrity conhosts because
/// modern headless ConPTY deliberately sandboxes them; control-pipe callers require
/// exact integrity so a sandboxed process cannot pause or stop a medium broker.
/// A lower broker can never bridge an elevated terminal.
fn verify_client(pipe: &NamedPipeServer, allow_lower_integrity: bool) -> Result<()> {
    let mut pid = 0u32;
    let handle = pipe.as_raw_handle() as HANDLE;
    // SAFETY: this is a connected named-pipe handle and `pid` is writable.
    if unsafe { GetNamedPipeClientProcessId(handle, &mut pid) } == 0 {
        bail!("could not identify named-pipe client process");
    }
    // SAFETY: PID came from the kernel for this connected pipe.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        bail!("could not open named-pipe client process");
    }
    let _process = HandleGuard(process);
    let client = process_token(process)?;
    // SAFETY: pseudo-handle is always valid in the current process.
    let current = process_token(unsafe { GetCurrentProcess() })?;
    let client_info = token_identity(client.0)?;
    let current_info = token_identity(current.0)?;
    // SAFETY: both SIDs point into live token-info allocations.
    if unsafe { EqualSid(client_info.user_sid(), current_info.user_sid()) } == 0 {
        bail!("named-pipe client belongs to another user");
    }
    let client_integrity = client_info.integrity_rid()?;
    let current_integrity = current_info.integrity_rid()?;
    if client_integrity > current_integrity {
        bail!("named-pipe client has a higher integrity level than the broker");
    }
    if !allow_lower_integrity && client_integrity != current_integrity {
        bail!("stream control client has a different integrity level");
    }
    Ok(())
}

fn process_token(process: HANDLE) -> Result<HandleGuard> {
    let mut token = std::ptr::null_mut();
    // SAFETY: valid process handle and output pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        bail!("opening process token failed");
    }
    Ok(HandleGuard(token))
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: owned non-null kernel handle, closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

struct TokenIdentity {
    user: TokenBuffer,
    integrity: TokenBuffer,
}

impl TokenIdentity {
    fn user_sid(&self) -> *mut core::ffi::c_void {
        // SAFETY: allocation was filled as TOKEN_USER and remains live.
        unsafe { (*(self.user.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }

    fn integrity_rid(&self) -> Result<u32> {
        // SAFETY: allocation was filled as TOKEN_MANDATORY_LABEL and remains live.
        let sid = unsafe {
            (*(self.integrity.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()))
                .Label
                .Sid
        };
        // SAFETY: token manager supplied a valid SID.
        let count = unsafe { *GetSidSubAuthorityCount(sid) };
        if count == 0 {
            bail!("token integrity SID has no subauthority");
        }
        // SAFETY: count came from this SID; last index is in bounds.
        Ok(unsafe { *GetSidSubAuthority(sid, u32::from(count - 1)) })
    }
}

fn token_identity(token: HANDLE) -> Result<TokenIdentity> {
    Ok(TokenIdentity {
        user: token_information(token, TokenUser)?,
        integrity: token_information(token, TokenIntegrityLevel)?,
    })
}

struct TokenBuffer(Vec<usize>);

impl TokenBuffer {
    fn as_ptr(&self) -> *const usize {
        self.0.as_ptr()
    }
}

fn token_information(
    token: HANDLE,
    class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<TokenBuffer> {
    let mut needed = 0u32;
    // SAFETY: null buffer/zero length is the documented size query.
    unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 || needed > 64 * 1024 {
        bail!("invalid process-token information size");
    }
    // TOKEN_* structures require pointer alignment, while Vec<u8> only promises
    // byte alignment. Allocate pointer-aligned words and expose their initialized
    // bytes for parsing.
    let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
    let mut aligned = vec![0usize; words];
    // SAFETY: aligned allocation has at least `needed` writable bytes.
    if unsafe {
        GetTokenInformation(
            token,
            class,
            aligned.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        bail!("reading process-token information failed");
    }
    // Keep the pointer-aligned allocation alive: TOKEN_USER/TOKEN_MANDATORY_LABEL
    // contain SID pointers that refer back into this same buffer.
    Ok(TokenBuffer(aligned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Frame;
    use crate::native_protocol::{MessageType, testwire};
    use tokio::net::windows::named_pipe::ClientOptions;

    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test(flavor = "multi_thread")]
    async fn detached_control_plane_pauses_reports_and_resumes() {
        let _guard = TEST_LOCK.lock().await;
        start_control_server().unwrap();
        assert_eq!(control("pause").await.unwrap(), "paused\n");
        let status = control("status").await.unwrap();
        assert!(status.starts_with("paused;"), "{status}");
        assert_eq!(control("resume").await.unwrap(), "streaming\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn named_pipe_mock_adapter_publishes_end_to_end() {
        let _guard = TEST_LOCK.lock().await;
        let name = format!(
            r"\\.\pipe\shellglass-render-tap-test-{}",
            std::process::id()
        );
        // The mock reports an explicit focus transition so sticky-last policy is
        // deterministic even when GetForegroundWindow is null on headless CI.
        let mut source = start_named(true, name.clone()).unwrap();
        let mut client = loop {
            match ClientOptions::new().open(&name) {
                Ok(client) => break client,
                Err(error) if error.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("opening test adapter pipe: {error}"),
            }
        };
        let nonce = 0x1234;
        client
            .write_all(&testwire::packet(
                MessageType::Hello,
                nonce,
                1,
                &testwire::hello(crate::native_protocol::Provider::Conhost),
            ))
            .await
            .unwrap();
        let hwnd = 1;
        client
            .write_all(&testwire::packet(
                MessageType::SourceAdded,
                nonce,
                2,
                &testwire::source_added(8, 1, hwnd),
            ))
            .await
            .unwrap();
        let mut update = Vec::new();
        update.extend_from_slice(&8u64.to_le_bytes());
        update.extend_from_slice(&1u64.to_le_bytes());
        update.push(12); // focused + visible
        update.push(1);
        update.push(1);
        client
            .write_all(&testwire::packet(
                MessageType::SourceUpdated,
                nonce,
                3,
                &update,
            ))
            .await
            .unwrap();

        // Selection sends SUBSCRIBE back over the same pipe.
        let mut command = vec![0u8; crate::native_protocol::ENVELOPE_LEN + 18];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut command))
            .await
            .expect("broker did not subscribe")
            .unwrap();
        assert_eq!(&command[..4], b"SGNT");
        assert_eq!(
            u16::from_le_bytes([command[6], command[7]]),
            MessageType::Subscribe as u16
        );

        client
            .write_all(&testwire::packet(
                MessageType::Frame,
                nonce,
                4,
                &testwire::frame(8, 1, 1, "z"),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), source.frames.changed())
            .await
            .expect("broker did not publish frame")
            .unwrap();
        let current = source.frames.borrow_and_update();
        let Frame::Screen(grid) = &**current;
        assert_eq!(grid.rows[0][0].text, "z");
    }
}
