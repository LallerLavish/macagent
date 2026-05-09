use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Semaphore};
use tracing::{debug, error, info, warn};

use crate::daemon::Engines;

// ---- Tunables ------------------------------------------------------------

/// Hard cap on a single line. ~64KB is plenty for any sane command and
/// orders of magnitude smaller than what an attacker would send.
pub const MAX_MESSAGE_BYTES: u64 = 64 * 1024;

/// Drop a connection that has been silent for this long.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Wrap each execute() call in this. Anything legitimately longer than this
/// (long shell commands, transcription) should be made async + return a
/// job_id rather than block the IPC reply.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// On shutdown, give in-flight handlers this long to finish their current
/// command before we drop the listener.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// On accept() error, sleep this long before retrying so we don't spin
/// the CPU during transient fd exhaustion.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Wire protocol version. Bump on breaking schema changes.
const PROTOCOL_VERSION: u32 = 1;

// ---- Configuration -------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ListenerConfig {
    pub max_concurrent_connections: usize,
    pub idle_timeout: Duration,
    pub command_timeout: Duration,
    pub max_message_bytes: u64,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_connections: 64,
            idle_timeout: IDLE_TIMEOUT,
            command_timeout: COMMAND_TIMEOUT,
            max_message_bytes: MAX_MESSAGE_BYTES,
        }
    }
}

// ---- Wire protocol -------------------------------------------------------
//
// We keep accepting plain text for backwards compat with `echo ... | nc -U`
// during development, but real clients should send JSON. Both are framed
// one-message-per-line.

/// Optional incoming JSON envelope. If a line parses as this, we use the
/// fields; otherwise we treat the whole line as a raw command string for
/// netcat-style testing.
#[derive(Debug, Deserialize)]
struct Request<'a> {
    #[serde(default)]
    id: Option<String>,
    #[serde(borrow)]
    command: &'a str,
}

#[derive(Debug, Serialize)]
struct Response<'a> {
    v: u32,
    id: Option<String>,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
}

impl<'a> Response<'a> {
    fn ok(id: Option<String>, intent_summary: IntentSummary, message: String) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            status: "success",
            action: Some(intent_summary.action),
            target: Some(intent_summary.target),
            confidence: Some(intent_summary.confidence),
            message: Some(message),
            error_kind: None,
        }
    }

    fn err(id: Option<String>, kind: &'static str, message: String) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            status: "error",
            action: None,
            target: None,
            confidence: None,
            message: Some(message),
            error_kind: Some(kind.to_string()),
        }
    }
}

/// Stable, serializable view of an Intent for the response. Avoids leaking
/// internal Debug formatting onto the wire.
struct IntentSummary {
    action: String,
    target: String,
    confidence: f32,
}

// ---- Listener entrypoint -------------------------------------------------

/// Bind the socket and run the accept loop until shutdown.
///
/// `engines` is cloned cheaply per connection; everything inside is Arc.
pub async fn start_listener(
    socket_path: &str,
    engines: Engines,
    cfg: ListenerConfig,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Caller (daemon.rs / setup code) is responsible for the umask + parent
    // directory permission dance described in the robustness pass. We just
    // bind here.
    if Path::new(socket_path).exists() {
        // Stale socket from a previous crashed run.
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;

    // Tighten the socket file's perms to 0600 immediately after bind.
    // The umask guard set by the caller already handled the race window;
    // this is belt-and-suspenders for clarity.
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;

    info!(socket = %socket_path, "IPC listener bound");

    let semaphore = Arc::new(Semaphore::new(cfg.max_concurrent_connections));
    let cfg = Arc::new(cfg);

    // Track active handlers so we can wait for them on shutdown.
    let tracker = tokio_util::task::TaskTracker::new();

    loop {
        tokio::select! {
            biased;

            // Shutdown takes priority over new accepts.
            _ = shutdown_rx.recv() => {
                info!("Listener received shutdown; closing socket and draining");
                break;
            }

            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _addr)) => {
                        // Validate peer UID (prevents other users on the box
                        // from talking to us). The robustness pass mentions
                        // this is also done at bind-time via socket perms,
                        // but we double-check per-connection in case mode
                        // bits drift.
                        if let Err(e) = check_peer_uid(&stream) {
                            warn!(error = %e, "rejecting connection from foreign UID");
                            drop(stream);
                            continue;
                        }

                        // Try to grab a connection slot. If we're over the
                        // cap, send a busy reply and close — never block
                        // accept().
                        let permit = match Arc::clone(&semaphore).try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                warn!(
                                    cap = cfg.max_concurrent_connections,
                                    "connection cap reached; rejecting"
                                );
                                tokio::spawn(send_busy_and_close(stream));
                                continue;
                            }
                        };

                        let engines = engines.clone();
                        let cfg = Arc::clone(&cfg);
                        let conn_shutdown = shutdown_rx.resubscribe();

                        tracker.spawn(async move {
                            let _permit = permit; // released on task exit
                            if let Err(e) = handle_connection(
                                stream,
                                engines,
                                cfg,
                                conn_shutdown,
                            ).await {
                                // Connection errors are local — log and
                                // move on. Never let one client kill the
                                // listener.
                                debug!(error = %e, "connection ended with error");
                            }
                        });
                    }
                    Err(e) if is_transient_accept_error(&e) => {
                        warn!(error = %e, "transient accept error; backing off");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                    }
                    Err(e) => {
                        error!(error = %e, "fatal accept error; listener exiting");
                        return Err(Box::new(e));
                    }
                }
            }
        }
    }

    // Stop accepting. Existing handlers run to completion (or until drain
    // timeout). Closing the listener here releases the bound fd; existing
    // streams are independent.
    drop(listener);
    let _ = std::fs::remove_file(socket_path);

    tracker.close();
    match tokio::time::timeout(DRAIN_TIMEOUT, tracker.wait()).await {
        Ok(()) => info!("All IPC connections drained cleanly"),
        Err(_) => warn!(
            timeout_secs = DRAIN_TIMEOUT.as_secs(),
            "Drain timeout exceeded; some connections were dropped"
        ),
    }

    Ok(())
}

fn is_transient_accept_error(e: &io::Error) -> bool {
    // EMFILE/ENFILE: out of file descriptors. ENOBUFS/ENOMEM: kernel pressure.
    // EINTR: signal. None are reasons to kill the listener.
    matches!(
        e.raw_os_error(),
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS)
            | Some(libc::ENOMEM) | Some(libc::EINTR) | Some(libc::ECONNABORTED)
    )
}

fn check_peer_uid(stream: &UnixStream) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // SO_PEERCRED equivalent on macOS is LOCAL_PEERCRED via getsockopt, but
    // for consumer UIDs the simpler getpeereid() is sufficient and stable.
    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: fd is valid for the lifetime of the borrow; uid/gid are
    // out-params written by the syscall.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getuid is always safe; returns current process uid.
    let our_uid = unsafe { libc::getuid() };
    if uid != our_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer uid {} != our uid {}", uid, our_uid),
        ));
    }
    Ok(())
}

async fn send_busy_and_close(mut stream: UnixStream) {
    let resp = Response::err(
        None,
        "Busy",
        "daemon at connection capacity; retry shortly".into(),
    );
    if let Ok(mut s) = serde_json::to_string(&resp) {
        s.push('\n');
        let _ = stream.write_all(s.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

// ---- Connection handler --------------------------------------------------

async fn handle_connection(
    stream: UnixStream,
    engines: Engines,
    cfg: Arc<ListenerConfig>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    debug!("IPC client connected");

    loop {
        line.clear();

        // Read a single line, bounded by max_message_bytes and idle_timeout,
        // and abortable by shutdown.
        let read_result = tokio::select! {
            biased;
            _ = shutdown_rx.recv() => {
                debug!("connection observing shutdown; closing");
                break;
            }
            r = tokio::time::timeout(
                cfg.idle_timeout,
                read_bounded_line(&mut buf_reader, &mut line, cfg.max_message_bytes),
            ) => r,
        };

        let bytes_read = match read_result {
            Ok(Ok(n)) => n,
            Ok(Err(ReadError::TooLarge)) => {
                let resp = Response::err(
                    None,
                    "MessageTooLarge",
                    format!("message exceeded {} bytes", cfg.max_message_bytes),
                );
                write_response(&mut writer, &resp).await?;
                // Drop the connection — we don't know where the next message
                // boundary is.
                break;
            }
            Ok(Err(ReadError::Io(e))) => return Err(Box::new(e)),
            Err(_elapsed) => {
                debug!(
                    timeout_secs = cfg.idle_timeout.as_secs(),
                    "idle timeout; closing connection"
                );
                break;
            }
        };

        if bytes_read == 0 {
            debug!("client disconnected");
            return Ok(());
        }

        let raw = line.trim_end_matches(['\n', '\r']);
        if raw.is_empty() {
            continue;
        }

        // Try JSON envelope first; fall back to raw text for nc testing.
        let (id, command) = match serde_json::from_str::<Request>(raw) {
            Ok(req) => (req.id, req.command.to_string()),
            Err(_) => (None, raw.to_string()),
        };

        debug!(id = ?id, "processing command");

        // Run parse + execute under a budget. Anything over command_timeout
        // is reported as a Timeout error rather than allowed to wedge the
        // connection's command loop.
        let response = match tokio::time::timeout(
            cfg.command_timeout,
            process_command(&engines, &command),
        )
        .await
        {
            Ok(results) => {
                // Combine all messages into one string so you can see everything that happened
                let combined_message = results.iter()
                    .map(|r| r.message.as_str())
                    .collect::<Vec<_>>()
                    .join(" AND ");
                
                // If ANY target failed, treat the overall response as an error
                if let Some(err) = results.iter().find(|r| r.error_kind.is_some()) {
                    Response::err(id.clone(), err.error_kind.unwrap(), combined_message)
                } else if let Some(first_result) = results.into_iter().next() {
                    // Success! Return the combined message
                    Response::ok(id.clone(), first_result.summary.unwrap(), combined_message)
                } else {
                    Response::err(id.clone(), "Empty", "No results returned".into())
                }
            }
            Err(_) => Response::err(
                id.clone(),
                "Timeout",
                format!(
                    "command exceeded {}s budget",
                    cfg.command_timeout.as_secs()
                ),
            ),
        };

        write_response(&mut writer, &response).await?;
    }

    let _ = writer.shutdown().await;
    Ok(())
}

struct CommandResult {
    summary: Option<IntentSummary>,
    message: String,
    error_kind: Option<&'static str>,
}

async fn process_command(
    engines: &Engines,
    raw: &str,
) -> Vec<CommandResult> {
    use crate::pipeline::{lexer, grammar, resolver};

    // Stage 1: tokenize. Always succeeds; empty input → empty token stream.
    let tokens = lexer::tokenize(raw);

    // Stage 2: parse tokens into AST. ParseError surfaces as a single
    // top-level failure for the whole command — there's no per-target
    // partial-success story when the structure itself is broken.
    let cmd = match grammar::parse(&tokens) {
        Ok(c) => c,
        Err(e) => {
            return vec![CommandResult {
                summary: None,
                message: e.to_string(),
                error_kind: Some(parse_error_kind(&e)),
            }];
        }
    };

    // Stage 3: resolve every phrase's targets via the registries. This
    // produces a flat list of per-target outcomes — Intents or typed errors.
    let outcomes = resolver::resolve_command(&cmd, &engines.apps);

    if outcomes.is_empty() {
        return vec![CommandResult {
            summary: None,
            message: "command resolved to no targets".to_string(),
            error_kind: Some("EmptyResolve"),
        }];
    }

    // Stage 4: execute each resolved Intent, collect each error in place.
    // Sequential for now (Phase 5 will parallelize via the planner).
    let mut results = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        match outcome {
            resolver::ResolvedTarget::Intent(intent) => {
                results.push(execute_one(engines, intent).await);
            }
            resolver::ResolvedTarget::Error(e) => {
                results.push(CommandResult {
                    summary: None,
                    message: e.to_string(),
                    error_kind: Some(e.kind_str()),
                });
            }
        }
    }
    results
}

async fn execute_one(engines: &Engines, intent: crate::engines::actions::intent::Intent) -> CommandResult {
    let summary = IntentSummary {
        action: format!("{:?}", intent.action),
        target: format!("{:?}", intent.target),
        confidence: intent.confidence_score(),
    };

    match engines.executor.execute(intent).await {
        Ok(msg) => CommandResult {
            summary: Some(summary),
            message: msg,
            error_kind: None,
        },
        Err(e) => CommandResult {
            summary: Some(summary),
            message: e.to_string(),
            error_kind: Some(e.kind_str()),
        },
    }
}

/// Stable wire tag for grammar parse errors.
fn parse_error_kind(e: &crate::pipeline::grammar::ParseError) -> &'static str {
    use crate::pipeline::grammar::ParseError::*;
    match e {
        Empty                => "EmptyCommand",
        NoVerb               => "NoVerb",
        UnexpectedToken(_)   => "UnexpectedToken",
        DanglingConjunction  => "DanglingConjunction",
        EmptyTarget          => "EmptyTarget",
        MultipleVerbs        => "MultipleVerbs",
        MultipleModifiers    => "MultipleModifiers",
    }
}

async fn write_response<W>(
    writer: &mut W,
    resp: &Response<'_>,
) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let mut buf = serde_json::to_vec(resp)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

// ---- Bounded line reader -------------------------------------------------

#[derive(Debug)]
enum ReadError {
    TooLarge,
    Io(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}

/// Read until newline, but never read more than `limit` bytes. If the limit
/// is hit before a newline, return TooLarge — caller must drop the
/// connection because we don't know where the next frame starts.
async fn read_bounded_line<R>(
    reader: &mut BufReader<R>,
    out: &mut String,
    limit: u64,
) -> Result<usize, ReadError>
where
    R: AsyncReadExt + Unpin,
{
    let mut taken = reader.take(limit);
    let mut tmp = Vec::with_capacity(256);
    let n = taken.read_until(b'\n', &mut tmp).await?;
    if n == 0 {
        return Ok(0);
    }
    // If we hit the limit and still didn't see a newline, this is oversized.
    if !tmp.ends_with(b"\n") && n as u64 == limit {
        return Err(ReadError::TooLarge);
    }
    match std::str::from_utf8(&tmp) {
        Ok(s) => {
            out.push_str(s);
            Ok(n)
        }
        Err(e) => Err(ReadError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            e,
        ))),
    }
}