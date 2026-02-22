use std::future;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::policy::WsPolicy;

const BACKOFF_MIN_SECS: u64 = 1;
const BACKOFF_MAX_SECS: u64 = 60;

/// Handle to a running [`ws_supervisor`] task.
///
/// Drop or call [`cancel`](WsSupervisorHandle::cancel) to shut the supervisor
/// down gracefully.
pub struct WsSupervisorHandle {
    handle: JoinHandle<()>,
    cancel: CancellationToken,
}

impl WsSupervisorHandle {
    /// Signal the supervisor to stop after the current message is processed.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Wait for the supervisor task to finish.
    pub async fn join(self) {
        let _ = self.handle.await;
    }
}

/// Spawn a supervised WebSocket task driven by `policy`.
///
/// The task runs a reconnect loop with exponential back-off:
/// 1. **`prepare()`** – obtain the URL (and any tokens); retry on error.
/// 2. **`connect_async()`** – open the TCP+TLS connection; retry on error.
///    Resets the back-off to `1 s` on first success.
/// 3. **`on_connected()`** – perform the post-connect handshake; reconnect on
///    error.
/// 4. **Message loop** – `select!` over incoming frames, heartbeat ticks, and
///    the cancellation token:
///    - Cancellation → close and return.
///    - Heartbeat tick → call `send_heartbeat()`; reconnect on error.
///    - Incoming text/binary frame → call `parse_message()`; log on error,
///      *do not* reconnect.
///    - Close/stream-end → break (reconnect).
pub fn ws_supervisor_spawn<P: WsPolicy>(
    policy: P,
    cancel: CancellationToken,
) -> WsSupervisorHandle {
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(run_supervisor(policy, cancel_clone));
    WsSupervisorHandle { handle, cancel }
}

async fn run_supervisor<P: WsPolicy>(mut policy: P, cancel: CancellationToken) {
    let mut backoff_secs = BACKOFF_MIN_SECS;

    loop {
        // ── Phase 1: prepare ──────────────────────────────────────────────────
        let url = loop {
            match policy.prepare().await {
                Ok(url) => {
                    debug!(url = %url, "WS prepare OK");
                    break url;
                }
                Err(e) => {
                    warn!(error = %e, backoff_secs, "WS prepare failed, retrying");
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            info!("WS supervisor cancelled during prepare; exiting");
                            return;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    }
                    backoff_secs = (backoff_secs * 2).min(BACKOFF_MAX_SECS);
                }
            }
        };

        let ws = match connect_async(&url).await {
            Ok((ws, _)) => {
                info!(url = %url, "WS connected");
                backoff_secs = BACKOFF_MIN_SECS; // reset on success
                ws
            }
            Err(e) => {
                warn!(url = %url, error = %e, backoff_secs, "WS connect failed, retrying");
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        info!("WS supervisor cancelled during connect; exiting");
                        return;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(BACKOFF_MAX_SECS);
                continue;
            }
        };

        let (mut sink, mut stream) = futures_util::StreamExt::split(ws);

        if let Err(e) = policy.on_connected(&mut sink, &mut stream).await {
            warn!(error = %e, "WS on_connected failed; reconnecting");
            backoff_secs = (backoff_secs * 2).min(BACKOFF_MAX_SECS);
            continue;
        }

        let heartbeat = policy.heartbeat_interval();

        // Use `pending()` when no application heartbeat is needed so the
        // select! arm is never ready (native ping/pong is handled by tungstenite).
        let mut interval: Option<tokio::time::Interval> = heartbeat.map(tokio::time::interval);

        let disconnected = loop {
            tokio::select! {
                biased;

                // Graceful shutdown
                _ = cancel.cancelled() => {
                    info!("WS supervisor cancelled; closing connection");
                    let _ = sink.send(Message::Close(None)).await;
                    return;
                }

                // Application heartbeat / maintenance
                _ = tick_or_never(&mut interval) => {
                    if let Err(e) = policy.send_heartbeat(&mut sink).await {
                        warn!(error = %e, "WS heartbeat failed; reconnecting");
                        break true;
                    }
                }

                msg = stream.next() => {
                    match msg {
                        Some(Ok(frame @ (Message::Text(_) | Message::Binary(_)))) => {
                            if let Err(e) = policy.parse_message(frame).await {
                                warn!(error = %e, "WS parse_message error (continuing)");
                            }
                        }
                        Some(Ok(Message::Close(frame))) => {
                            info!(reason = ?frame, "WS connection closed by server");
                            break true;
                        }
                        Some(Ok(_)) => {
                            // Ping/Pong are handled by tungstenite; ignore here.
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "WS receive error; reconnecting");
                            break true;
                        }
                        None => {
                            info!("WS stream ended; reconnecting");
                            break true;
                        }
                    }
                }
            }
        };

        if disconnected {
            // Brief pause before reconnect so we don't hammer the server.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("WS supervisor cancelled before reconnect; exiting");
                    return;
                }
                _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
            }
            backoff_secs = (backoff_secs * 2).min(BACKOFF_MAX_SECS);
        }
    }
}

/// Returns a future that completes on the next interval tick, or is pending
/// forever if `interval` is `None`.
async fn tick_or_never(interval: &mut Option<tokio::time::Interval>) {
    match interval {
        Some(iv) => {
            iv.tick().await;
        }
        None => future::pending::<()>().await,
    }
}
