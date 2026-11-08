//! Browser userscript claim tracking and coordination HTTP server.

use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Local HTTP coordination server port for userscripts.
const SERVER_PORT: u16 = 38400;

/// Time-to-live duration for an active userscript claim heartbeat.
const CLAIM_TTL: Duration = Duration::from_millis(4000);

/// Reusable HTTP 200 OK response with CORS headers.
const HTTP_OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\
Access-Control-Allow-Origin: *\r\n\
Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
Access-Control-Allow-Headers: *\r\n\
Content-Length: 0\r\n\
Connection: close\r\n\r\n";

/// Tracks applications claimed by in-tab userscripts per tab ID.
#[derive(Default)]
pub struct ClaimTracker {
    /// Map of claimed application -> (map of tab_id -> last_heartbeat).
    claims: Mutex<HashMap<String, HashMap<String, Instant>>>,
}

impl ClaimTracker {
    /// Removes expired tab claims from the map.
    fn prune_expired(claims: &mut HashMap<String, HashMap<String, Instant>>) {
        claims.retain(|_, tabs| {
            tabs.retain(|_, time| time.elapsed() < CLAIM_TTL);
            !tabs.is_empty()
        });
    }

    /// Checks whether an executable name is currently claimed by any active
    /// tab.
    pub fn is_claimed(&self, exe_name: &str) -> bool {
        let exe_lower = exe_name.to_lowercase();
        let mut guard = self.claims.lock().unwrap();
        Self::prune_expired(&mut guard);
        guard.keys().any(|claimed| exe_lower.contains(claimed))
    }

    /// Checks whether any applications or browser tabs are currently active.
    pub fn has_any_claims(&self) -> bool {
        let mut guard = self.claims.lock().unwrap();
        Self::prune_expired(&mut guard);
        !guard.is_empty()
    }

    /// Records or refreshes a claim for a specific application tab.
    pub fn record_claim(&self, app: &str, tab_id: &str) {
        let app_lower = app.to_lowercase();
        let mut guard = self.claims.lock().unwrap();
        let tabs = guard.entry(app_lower).or_default();
        let was_empty = tabs.is_empty();
        tabs.insert(tab_id.to_string(), Instant::now());
        if was_empty {
            log::info!("'{app}' claimed by in-tab userscript (bypassing OS master pan)");
        }
    }

    /// Explicitly releases a claim when a specific tab pauses or closes.
    pub fn release_claim(&self, app: &str, tab_id: &str) {
        let app_lower = app.to_lowercase();
        let mut guard = self.claims.lock().unwrap();
        if let Some(tabs) = guard.get_mut(&app_lower) {
            tabs.remove(tab_id);
            if tabs.is_empty() {
                guard.remove(&app_lower);
                log::info!("'{app}' released by userscripts (resuming OS master pan)");
            }
        }
    }
}

/// Spawns a background TCP server to receive claim and release heartbeats from
/// userscripts.
pub fn start_claim_server(
    tracker: Arc<ClaimTracker>,
    running: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let listener = match TcpListener::bind(("127.0.0.1", SERVER_PORT)) {
        Ok(l) => l,
        Err(err) => {
            log::warn!(
                "Could not bind HTTP claim server on port {SERVER_PORT}: {err}. Userscript coordination disabled."
            );
            return Ok(());
        }
    };
    listener.set_nonblocking(true)?;

    thread::spawn(move || {
        log::info!("Userscript coordination server listening on http://127.0.0.1:{SERVER_PORT}");
        let mut buf = [0u8; 1024];
        while running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Sockets accepted from a non-blocking listener inherit
                    // non-blocking mode on Windows.
                    // Switch to blocking with a brief timeout so read waits for
                    // the HTTP request bytes.
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);

                    let app = if req.contains("app=firefox") {
                        "firefox"
                    } else if req.contains("app=msedge") || req.contains("app=edge") {
                        "msedge"
                    } else if req.contains("app=brave") {
                        "brave"
                    } else {
                        "chrome"
                    };

                    let tab_id = if let Some(idx) = req.find("tab=") {
                        let rest = &req[idx + 4..];
                        let end = rest.find(['&', ' ', '\r', '\n']).unwrap_or(rest.len());
                        &rest[..end]
                    } else {
                        "default"
                    };

                    let should_respond = if req.contains("/claim") {
                        tracker.record_claim(app, tab_id);
                        true
                    } else if req.contains("/release") {
                        tracker.release_claim(app, tab_id);
                        true
                    } else {
                        req.contains("/ping") || req.starts_with("OPTIONS")
                    };

                    if should_respond {
                        let _ = stream.write_all(HTTP_OK_RESPONSE);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(40));
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(40));
                }
            }
        }
    });

    Ok(())
}
