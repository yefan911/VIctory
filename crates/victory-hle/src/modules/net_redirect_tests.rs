//! Real WinHTTP requests between two synthetic loopback listeners only.
//! No external destination, request body, title data, or credentials.

use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

struct LocalServer {
    port: u16,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LocalServer {
    fn new(response: String) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, done) = (connections.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let (mut stream, peer) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("local listener: {e}"),
                };
                assert!(peer.ip().is_loopback());
                count.fetch_add(1, Ordering::Release);
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut bytes = [0u8; 1024];
                    let n = stream.read(&mut bytes).unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&bytes[..n]);
                    assert!(request.len() <= 4096);
                }
                assert!(request.starts_with(b"GET /"));
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        Self {
            port,
            connections,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

struct HttpHandle(Handle);

impl HttpHandle {
    fn new(handle: Handle) -> Self {
        assert!(!handle.is_null());
        Self(handle)
    }
}

impl Drop for HttpHandle {
    fn drop(&mut self) {
        host!("winhttp.dll", "WinHttpCloseHandle": extern "system" fn(Handle) -> i32)(self.0);
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn query_policy(handle: Handle) -> u32 {
    let (mut policy, mut len) = (u32::MAX, 4u32);
    let query = host!("winhttp.dll", "WinHttpQueryOption": extern "system" fn(Handle, u32, *mut c_void, *mut u32) -> i32);
    assert_ne!(
        query(
            handle,
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&raw mut policy).cast(),
            &mut len
        ),
        0
    );
    assert_eq!(len, 4);
    policy
}

fn try_policy(handle: Handle, policy: u32) {
    assert_ne!(
        WinHttpSetOption(
            handle,
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&raw const policy).cast(),
            4
        ),
        0
    );
}

fn exchange(
    port: u16,
    session_policy: Option<u32>,
    request_policy: Option<u32>,
) -> (u32, u32, u32) {
    let session = HttpHandle::new(WinHttpOpen(
        wide("VIctory loopback regression").as_ptr(),
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        std::ptr::null(),
        std::ptr::null(),
        0,
    ));
    let timeouts = host!("winhttp.dll", "WinHttpSetTimeouts": extern "system" fn(Handle, i32, i32, i32, i32) -> i32);
    assert_ne!(timeouts(session.0, 2000, 2000, 2000, 2000), 0);
    if let Some(policy) = session_policy {
        try_policy(session.0, policy);
    }
    let connection = HttpHandle::new(WinHttpConnect(
        session.0,
        wide("127.0.0.1").as_ptr(),
        port,
        0,
    ));
    let open = host!("winhttp.dll", "WinHttpOpenRequest": extern "system" fn(Handle, *const u16, *const u16, *const u16, *const u16, *const *const u16, u32) -> Handle);
    let request = HttpHandle::new(open(
        connection.0,
        wide("GET").as_ptr(),
        wide("/redirect").as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
    ));
    if let Some(policy) = request_policy {
        try_policy(request.0, policy);
    }
    let send = host!("winhttp.dll", "WinHttpSendRequest": extern "system" fn(Handle, *const u16, u32, *const c_void, u32, u32, usize) -> i32);
    let receive = host!("winhttp.dll", "WinHttpReceiveResponse": extern "system" fn(Handle, *mut c_void) -> i32);
    assert_ne!(
        send(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0),
        0
    );
    assert_ne!(receive(request.0, std::ptr::null_mut()), 0);
    let query = host!("winhttp.dll", "WinHttpQueryHeaders": extern "system" fn(Handle, u32, *const u16, *mut c_void, *mut u32, *mut u32) -> i32);
    let (mut status, mut len) = (0u32, 4u32);
    // WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER from winhttp.h.
    assert_ne!(
        query(
            request.0,
            19 | 0x2000_0000,
            std::ptr::null(),
            (&raw mut status).cast(),
            &mut len,
            std::ptr::null_mut()
        ),
        0
    );
    (status, query_policy(session.0), query_policy(request.0))
}

#[test]
fn localhost_redirects_are_returned_without_following_or_policy_relaxation() {
    let target = LocalServer::new(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    );
    let origin = LocalServer::new(format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{}/target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        target.port
    ));
    // 1 = DISALLOW_HTTPS_TO_HTTP (the host default), 2 = ALWAYS. Neither
    // may relax our policy, whether applied to the session or the request.
    let observed: Vec<_> = [
        (None, None),
        (Some(2), None),
        (None, Some(2)),
        (Some(2), Some(1)),
    ]
    .into_iter()
    .map(|(session, request)| exchange(origin.port, session, request))
    .collect();
    let origins = origin.connections.load(Ordering::Acquire);
    let targets = target.connections.load(Ordering::Acquire);
    eprintln!(
        "Loopback origin connections={origins}, redirect target connections={targets}, responses/policies={observed:?}"
    );
    assert_eq!(origins, 4);
    assert_eq!(
        targets, 0,
        "the second loopback listener must never be contacted"
    );
    assert_eq!(
        observed,
        vec![
            (
                307,
                WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
                WINHTTP_OPTION_REDIRECT_POLICY_NEVER
            );
            4
        ]
    );
}

#[test]
fn dynamically_loaded_redirect_controls_use_the_guards() {
    use crate::modules::kernel::{FreeLibrary, GetProcAddress, LoadLibraryA};
    let module = LoadLibraryA(c"winhttp.dll".as_ptr().cast());
    assert!(!module.is_null());
    for (name, guarded) in [
        (c"WinHttpOpen", WinHttpOpen as *const () as usize),
        (c"WinHttpSetOption", WinHttpSetOption as *const () as usize),
    ] {
        assert_eq!(
            GetProcAddress(module, name.as_ptr().cast()) as usize,
            guarded
        );
    }
    assert_eq!(FreeLibrary(module), 1);
}

#[test]
fn redirect_policy_failure_is_not_reported_as_success() {
    // Other tests may open WinHTTP handles in parallel. Isolate this closed-
    // handle check so its value cannot be reused between close and query.
    const CHILD: &str = "VICTORY_TEST_WINHTTP_POLICY_FAILURE";
    if std::env::var(CHILD).as_deref() != Ok("1") {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "modules::net::redirect_tests::redirect_policy_failure_is_not_reported_as_success",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "child ran no test: {stdout}");
        return;
    }
    let policy = 2u32;
    assert_eq!(
        WinHttpSetOption(
            std::ptr::null_mut(),
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&raw const policy).cast(),
            4
        ),
        0
    );
    // SAFETY: reads only this thread's error.
    assert_ne!(unsafe { windows_sys::Win32::Foundation::GetLastError() }, 0);

    // A connection is a real WinHTTP handle, but the redirect policy is only
    // supported on sessions and requests. This exercises failure after a
    // non-null handle exists, without issuing any HTTP request.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let session = HttpHandle::new(WinHttpOpen(
        wide("VIctory option failure regression").as_ptr(),
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        std::ptr::null(),
        std::ptr::null(),
        0,
    ));
    let connection = WinHttpConnect(
        session.0,
        wide("127.0.0.1").as_ptr(),
        listener.local_addr().unwrap().port(),
        0,
    );
    assert!(!connection.is_null());
    let query = host!("winhttp.dll", "WinHttpQueryOption": extern "system" fn(Handle, u32, *mut c_void, *mut u32) -> i32);
    let (mut kind, mut len) = (0u32, 4u32);
    // WINHTTP_OPTION_HANDLE_TYPE = 9, WINHTTP_HANDLE_TYPE_CONNECT = 2.
    assert_ne!(query(connection, 9, (&raw mut kind).cast(), &mut len), 0);
    assert_eq!(kind, 2);
    assert_eq!(
        WinHttpSetOption(
            connection,
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&raw const policy).cast(),
            4
        ),
        0
    );
    // The option failure survives the close; querying the same handle then
    // confirms that it no longer remains usable. Do not close it again.
    // SAFETY: reads only this thread's error (INCORRECT_HANDLE_TYPE).
    assert_eq!(unsafe { GetLastError() }, 12018);
    assert_eq!(query(connection, 9, (&raw mut kind).cast(), &mut len), 0);
    // SAFETY: reads only this thread's error (INVALID_HANDLE).
    assert_eq!(unsafe { GetLastError() }, 6);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
