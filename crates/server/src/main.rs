use azookey_server::{PipeConnectInfo, TonicNamedPipeServer};
use tonic::{transport::Server, Request, Response, Status};
use tonic_reflection::server::Builder as ReflectionBuilder;

use shared::proto::azookey_service_server::{AzookeyService, AzookeyServiceServer};
use shared::proto::{
    AppendTextRequest, AppendTextResponse, ClearTextRequest, ClearTextResponse, ComposingText,
    MoveCursorRequest, MoveCursorResponse, RemoveTextRequest, RemoveTextResponse,
    ShrinkTextRequest, ShrinkTextResponse, Suggestion,
};

use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

struct RawComposingText {
    text: String,
    // provided by the engine but not yet exposed over gRPC (the client's
    // MoveCursor handling is still a TODO)
    #[allow(dead_code)]
    cursor: i8,
}

#[derive(Debug, Clone)]
#[repr(C)]
struct FFICandidate {
    text: *mut c_char,
    subtext: *mut c_char,
    hiragana: *mut c_char,
    corresponding_count: c_int,
}

// FFI contract with the Swift engine (azookey_server.swift):
// - all functions must be called from a single thread, serially — the Swift
//   side keeps unsynchronized global state (see the current_thread runtime
//   in main())
// - out-parameters are 32-bit (c_int on both sides)
// - `session` selects the per-client composing state; it is the pipe
//   connection id assigned in lib.rs (see PipeConnectInfo)
// - every returned string/list is owned by the callee and must be handed
//   back to FreeString / FreeComposedText after copying
unsafe extern "C" {
    fn Initialize(path: *const c_char);
    fn SetContext(session: c_int, context: *const c_char);
    fn AppendText(session: c_int, input: *const c_char, cursorPtr: *mut c_int) -> *mut c_char;
    fn RemoveText(session: c_int, cursorPtr: *mut c_int) -> *mut c_char;
    fn MoveCursor(session: c_int, offset: c_int, cursorPtr: *mut c_int) -> *mut c_char;
    fn ShrinkText(session: c_int, offset: c_int) -> *mut c_char;
    fn ClearText(session: c_int);
    fn GetComposedText(session: c_int, lengthPtr: *mut c_int) -> *mut *mut FFICandidate;
    fn RemoveSession(session: c_int);
    fn LoadConfig();
    fn FreeString(ptr: *mut c_char);
    fn FreeComposedText(listPtr: *mut *mut FFICandidate, length: c_int);
}

/// Sessions are created implicitly on first use, but nothing tells the
/// server when a client connection goes away — evict engine state that has
/// been idle for a while so exited applications don't accumulate sessions.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

static SESSION_LAST_USED: LazyLock<Mutex<HashMap<i32, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Extracts the pipe-connection session id tonic stored in the request.
fn session_of<T>(request: &Request<T>) -> i32 {
    let id = request
        .extensions()
        .get::<PipeConnectInfo>()
        .map(|info| info.session_id)
        .unwrap_or(0);
    touch_session(id);
    id
}

fn touch_session(id: i32) {
    let mut map = SESSION_LAST_USED
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    map.insert(id, Instant::now());

    let expired: Vec<i32> = map
        .iter()
        .filter(|(sid, last)| **sid != id && last.elapsed() > SESSION_IDLE_TIMEOUT)
        .map(|(sid, _)| *sid)
        .collect();
    for sid in expired {
        map.remove(&sid);
        unsafe { RemoveSession(sid) };
    }
}

/// Builds a CString from possibly untrusted input. Interior NUL bytes cannot
/// be represented in a C string, so they are stripped instead of panicking —
/// requests arrive over a pipe any local process can open.
fn to_cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Copies a C string returned by the Swift engine and frees the original.
/// Tolerates null pointers and invalid UTF-8 instead of crashing the server.
unsafe fn consume_cstr(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        let text = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { FreeString(ptr) };
        text
    }
}

fn initialize(path: &str) {
    unsafe {
        let path = to_cstring(path);
        Initialize(path.as_ptr());
    }
}

fn add_text(session: i32, input: &str) -> RawComposingText {
    unsafe {
        let input = to_cstring(input);
        let mut cursor: c_int = 0;

        let result = AppendText(session, input.as_ptr(), &mut cursor);
        let text = consume_cstr(result);

        RawComposingText {
            text,
            cursor: cursor as i8,
        }
    }
}

fn move_cursor(session: i32, offset: i8) -> RawComposingText {
    unsafe {
        let offset = c_int::from(offset);
        let mut cursor: c_int = 0;

        let result = MoveCursor(session, offset, &mut cursor);
        let text = consume_cstr(result);

        RawComposingText {
            text,
            cursor: cursor as i8,
        }
    }
}

fn remove_text(session: i32) -> RawComposingText {
    unsafe {
        let mut cursor: c_int = 0;

        let result = RemoveText(session, &mut cursor);
        let text = consume_cstr(result);

        RawComposingText {
            text,
            cursor: cursor as i8,
        }
    }
}

fn clear_text(session: i32) {
    unsafe {
        ClearText(session);
    }
}

/// Copies a C string without taking ownership (the containing candidate
/// list is freed as a whole by FreeComposedText).
unsafe fn cstr_or_empty(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
    }
}

fn get_composed_text(session: i32) -> Vec<Suggestion> {
    unsafe {
        let mut length: c_int = 0;
        let result = GetComposedText(session, &mut length);

        if result.is_null() {
            return Vec::new();
        }

        let mut suggestions = Vec::with_capacity(length.max(0) as usize);

        for index in 0..length.max(0) as usize {
            let candidate_ptr = *result.add(index);
            if candidate_ptr.is_null() {
                continue;
            }
            let candidate = (*candidate_ptr).clone();
            let text = cstr_or_empty(candidate.text);
            let subtext = cstr_or_empty(candidate.subtext);
            let corresponding_count = candidate.corresponding_count;

            let suggestion = Suggestion {
                text,
                subtext,
                corresponding_count,
            };

            // check if suggestions have the same text
            if suggestions
                .iter()
                .any(|s: &Suggestion| s.text == suggestion.text)
            {
                continue;
            }
            suggestions.push(suggestion);
        }

        FreeComposedText(result, length.max(0));

        suggestions
    }
}

fn shrink_text(session: i32, offset: i8) -> RawComposingText {
    unsafe {
        let offset = c_int::from(offset);
        let result = ShrinkText(session, offset);
        let text = consume_cstr(result);

        RawComposingText { text, cursor: 0 }
    }
}

#[derive(Debug, Default)]
pub struct MyAzookeyService;

#[tonic::async_trait]
impl AzookeyService for MyAzookeyService {
    async fn append_text(
        &self,
        request: Request<AppendTextRequest>,
    ) -> Result<Response<AppendTextResponse>, Status> {
        let session = session_of(&request);
        let input = request.into_inner().text_to_append;
        let composing_text = add_text(session, &input);

        Ok(Response::new(AppendTextResponse {
            composing_text: Some(ComposingText {
                hiragana: composing_text.text,
                suggestions: get_composed_text(session).to_vec(),
            }),
        }))
    }

    async fn remove_text(
        &self,
        request: Request<RemoveTextRequest>,
    ) -> Result<Response<RemoveTextResponse>, Status> {
        let session = session_of(&request);
        let composing_text = remove_text(session);

        Ok(Response::new(RemoveTextResponse {
            composing_text: Some(ComposingText {
                hiragana: composing_text.text,
                suggestions: get_composed_text(session).to_vec(),
            }),
        }))
    }

    async fn move_cursor(
        &self,
        request: Request<MoveCursorRequest>,
    ) -> Result<Response<MoveCursorResponse>, Status> {
        let session = session_of(&request);
        let offset = request.into_inner().offset as i8;
        let composing_text = move_cursor(session, offset);

        Ok(Response::new(MoveCursorResponse {
            composing_text: Some(ComposingText {
                hiragana: composing_text.text,
                suggestions: get_composed_text(session).to_vec(),
            }),
        }))
    }

    async fn clear_text(
        &self,
        request: Request<ClearTextRequest>,
    ) -> Result<Response<ClearTextResponse>, Status> {
        let session = session_of(&request);
        clear_text(session);
        Ok(Response::new(ClearTextResponse {}))
    }

    async fn shrink_text(
        &self,
        request: Request<ShrinkTextRequest>,
    ) -> Result<Response<ShrinkTextResponse>, Status> {
        let session = session_of(&request);
        let offset = request.into_inner().offset as i8;
        let composing_text = shrink_text(session, offset);

        Ok(Response::new(ShrinkTextResponse {
            composing_text: Some(ComposingText {
                hiragana: composing_text.text,
                suggestions: get_composed_text(session).to_vec(),
            }),
        }))
    }

    async fn set_context(
        &self,
        request: Request<shared::proto::SetContextRequest>,
    ) -> Result<Response<shared::proto::SetContextResponse>, Status> {
        let session = session_of(&request);
        let context = request.into_inner().context;
        let trimmed_context = context
            .split('\r')
            .rfind(|s| !s.is_empty())
            .unwrap_or_default();

        let context = to_cstring(trimmed_context);

        unsafe { SetContext(session, context.as_ptr()) };
        Ok(Response::new(shared::proto::SetContextResponse {}))
    }

    async fn update_config(
        &self,
        _: Request<shared::proto::UpdateConfigRequest>,
    ) -> Result<Response<shared::proto::UpdateConfigResponse>, Status> {
        unsafe { LoadConfig() };
        Ok(Response::new(shared::proto::UpdateConfigResponse {}))
    }
}

// The Swift engine keeps @MainActor global state and its FFI exports are not
// thread-safe. A single-threaded runtime serializes every FFI call onto one
// OS thread; the default multi-threaded runtime crashes inside dispatch.dll.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("AzookeyServer started");
    // get executable directory
    let current_exe = std::env::current_exe()?;
    let parent_dir = current_exe
        .parent()
        .ok_or("executable path has no parent directory")?;
    initialize(&parent_dir.to_string_lossy());

    let service = MyAzookeyService;

    // standard gRPC health service, polled by the launcher's watchdog.
    // Because this runtime is single-threaded, ANY hang inside a Swift FFI
    // call also blocks the health check — a trivial ping detects engine
    // hangs without touching the engine.
    // "" is the gRPC health protocol's "overall server" service name; the
    // launcher checks that instead of a per-service name
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    // test hook for the watchdog: block the (single-threaded) runtime after
    // N seconds so every RPC — including the health check — stops answering.
    // Completely inert unless the env var is set.
    if let Some(secs) = std::env::var("AZOOKEY_TEST_HANG_AFTER_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        eprintln!("TEST MODE: runtime will hang after {secs}s");
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            eprintln!("TEST MODE: blocking the runtime now");
            std::thread::sleep(std::time::Duration::MAX);
        });
    }

    println!("AzookeyServer listening");

    Server::builder()
        .add_service(health_service)
        .add_service(AzookeyServiceServer::new(service))
        .add_service(
            ReflectionBuilder::configure()
                .register_encoded_file_descriptor_set(shared::proto::FILE_DESCRIPTOR_SET)
                .build_v1()?,
        )
        .serve_with_incoming(TonicNamedPipeServer::new("azookey_server")?)
        .await?;

    Ok(())
}
