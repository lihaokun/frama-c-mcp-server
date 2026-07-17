use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

use super::codec::{self, FramaCCommand, FramaCResponse};
use super::transport::Transport;
use crate::error::FramaCError;
use crate::state::SessionState;

struct ClientInner {
    transport: Transport,
    counter: u64,
}

impl ClientInner {
    fn next_id(&mut self) -> String {
        let id = format!("RQ.{}", self.counter);
        self.counter += 1;
        id
    }

    async fn send_command(&mut self, cmd: &FramaCCommand) -> Result<(), FramaCError> {
        let json = codec::encode_command(cmd);
        self.transport.send_frame(&json).await
    }

    async fn recv_response(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<FramaCResponse>, FramaCError> {
        match self.transport.recv_frame(timeout).await? {
            Some(s) => Ok(Some(codec::decode_response(&s)?)),
            None => Ok(None),
        }
    }

    /// Read responses until the one matching `request_id` is received.
    /// Skips SIGNAL, CMDLINE, and responses for other request IDs (stale
    /// responses from timed-out operations).
    async fn wait_for_id(
        &mut self,
        request_id: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, FramaCError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(FramaCError::Timeout(timeout));
            }
            match self.recv_response(remaining).await? {
                Some(FramaCResponse::Data { id, data }) if id == request_id => {
                    return Ok(data);
                }
                Some(FramaCResponse::Error { id, msg }) if id == request_id => {
                    return Err(FramaCError::ServerError { id, msg });
                }
                Some(FramaCResponse::Rejected { id }) if id == request_id => {
                    return Err(FramaCError::Rejected { id });
                }
                // Skip signals and CMDLINE responses
                Some(FramaCResponse::Signal { .. })
                | Some(FramaCResponse::CmdLineOn)
                | Some(FramaCResponse::CmdLineOff) => continue,
                // Skip stale responses from other request IDs
                Some(FramaCResponse::Data { id, .. })
                | Some(FramaCResponse::Error { id, .. })
                | Some(FramaCResponse::Rejected { id })
                | Some(FramaCResponse::Killed { id }) => {
                    tracing::warn!(
                        "discarding stale response for {}, waiting for {}",
                        id,
                        request_id
                    );
                    continue;
                }
                None => return Err(FramaCError::Timeout(timeout)),
            }
        }
    }

    async fn poll_loop(
        &mut self,
        request_id: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, FramaCError> {
        let deadline = Instant::now() + timeout;

        // First, check if the server already responded before we start polling.
        // EXEC responses may arrive immediately for fast operations.
        if let Some(resp) = self.recv_response(Duration::from_millis(500)).await? {
            match resp {
                FramaCResponse::Data { id, data } if id == request_id => return Ok(data),
                FramaCResponse::Error { id, msg } if id == request_id => {
                    return Err(FramaCError::ServerError { id, msg });
                }
                FramaCResponse::Rejected { id } if id == request_id => {
                    return Err(FramaCError::Rejected { id });
                }
                FramaCResponse::Signal { .. }
                | FramaCResponse::CmdLineOn
                | FramaCResponse::CmdLineOff => {
                    // fall through to poll loop
                }
                other => {
                    tracing::warn!("unexpected initial response for EXEC: {:?}", other);
                }
            }
        }

        loop {
            if Instant::now() >= deadline {
                self.send_command(&FramaCCommand::Kill {
                    id: request_id.to_string(),
                })
                .await?;
                return Err(FramaCError::Timeout(timeout));
            }

            tokio::time::sleep(Duration::from_millis(100)).await;

            self.send_command(&FramaCCommand::Poll).await?;

            let resp = self
                .recv_response(Duration::from_millis(500))
                .await?;
            match resp {
                Some(FramaCResponse::Data { id, data }) if id == request_id => {
                    return Ok(data);
                }
                Some(FramaCResponse::Error { id, msg }) if id == request_id => {
                    return Err(FramaCError::ServerError { id, msg });
                }
                Some(FramaCResponse::Killed { id }) if id == request_id => {
                    return Err(FramaCError::Killed { id });
                }
                Some(FramaCResponse::Rejected { id }) if id == request_id => {
                    return Err(FramaCError::Rejected { id });
                }
                Some(FramaCResponse::Signal { .. }) => continue,
                Some(FramaCResponse::CmdLineOn) | Some(FramaCResponse::CmdLineOff) => continue,
                Some(other) => {
                    tracing::warn!("unexpected response during POLL: {:?}", other);
                    continue;
                }
                None => continue,
            }
        }
    }
}

pub struct FramaCClient {
    inner: Mutex<ClientInner>,
}

impl FramaCClient {
    pub async fn connect(
        path: &str,
        state: Arc<RwLock<SessionState>>,
    ) -> Result<Self, FramaCError> {
        let transport = Transport::connect(path).await?;
        let mut inner = ClientInner {
            transport,
            counter: 0,
        };

        // Handshake: Frama-C Server doesn't push data until the client
        // sends a command. Send a probe GET to trigger the server to flush
        // queued signals (CMDLINEON/CMDLINEOFF) along with the response.
        let probe_id = inner.next_id();
        inner
            .send_command(&FramaCCommand::Get {
                id: probe_id.clone(),
                request: "kernel.ast.getFiles".to_string(),
                data: serde_json::Value::Null,
            })
            .await?;

        // Read responses until we see CMDLINEOFF (max 30 seconds).
        // The server batches CMDLINEOFF with request responses, so we may
        // receive DATA for our probe GET interleaved with CMDLINE signals.
        let mut cmdlineoff_seen = false;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(FramaCError::ConnectTimeout);
            }
            match inner.recv_response(remaining).await? {
                Some(FramaCResponse::CmdLineOff) => {
                    cmdlineoff_seen = true;
                    break;
                }
                Some(FramaCResponse::CmdLineOn) => continue,
                Some(FramaCResponse::Data { .. }) => {
                    // Probe GET response — consume it, keep waiting for CMDLINEOFF
                    // CMDLINEOFF may arrive in the same batch (next frame)
                    continue;
                }
                Some(other) => {
                    tracing::warn!("unexpected during handshake: {:?}", other);
                    continue;
                }
                None => {
                    // Timeout reading — if we already got a Data response but
                    // no CMDLINEOFF, the server may have sent CMDLINEOFF before
                    // the command line phase (already past it). Treat as ready.
                    break;
                }
            }
        }
        if !cmdlineoff_seen {
            tracing::warn!("CMDLINEOFF not received, proceeding anyway");
        }

        let client = FramaCClient {
            inner: Mutex::new(inner),
        };

        // Auto-fetch function info to populate marker cache
        let entries = client
            .fetch_all("kernel.ast.fetchFunctions")
            .await?;
        {
            let mut st = state.write().await;
            st.update_functions(&entries);
            st.project_loaded = true;
        }

        Ok(client)
    }

    /// Reconnect to a new Frama-C server, replacing the transport.
    pub async fn reconnect(
        &self,
        path: &str,
        state: Arc<RwLock<SessionState>>,
    ) -> Result<(), FramaCError> {
        let transport = Transport::connect(path).await?;
        let mut new_inner = ClientInner {
            transport,
            counter: 0,
        };

        // Handshake (same as connect)
        let probe_id = new_inner.next_id();
        new_inner
            .send_command(&FramaCCommand::Get {
                id: probe_id.clone(),
                request: "kernel.ast.getFiles".to_string(),
                data: serde_json::Value::Null,
            })
            .await?;

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { break; }
            match new_inner.recv_response(remaining).await? {
                Some(FramaCResponse::CmdLineOff) => break,
                Some(FramaCResponse::CmdLineOn) => continue,
                Some(FramaCResponse::Data { .. }) => continue,
                Some(_) => continue,
                None => break,
            }
        }

        // Replace inner transport
        let mut inner = self.inner.lock().await;
        *inner = new_inner;

        // Re-fetch functions
        drop(inner); // release lock before calling self methods
        let entries = self.fetch_all("kernel.ast.fetchFunctions").await?;
        {
            let mut st = state.write().await;
            st.update_functions(&entries);
            st.project_loaded = true;
        }

        Ok(())
    }

    pub async fn get(
        &self,
        request: &str,
        data: serde_json::Value,
    ) -> Result<serde_json::Value, FramaCError> {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id();
        inner
            .send_command(&FramaCCommand::Get {
                id: id.clone(),
                request: request.to_string(),
                data,
            })
            .await?;
        inner.wait_for_id(&id, Duration::from_secs(10)).await
    }

    pub async fn set(
        &self,
        request: &str,
        data: serde_json::Value,
    ) -> Result<serde_json::Value, FramaCError> {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id();
        inner
            .send_command(&FramaCCommand::Set {
                id: id.clone(),
                request: request.to_string(),
                data,
            })
            .await?;
        // SET is queued (like EXEC), not processed immediately (like GET).
        // Use poll_loop to repeatedly send POLL until the server processes
        // the queue and responds with DATA.
        inner.poll_loop(&id, Duration::from_secs(30)).await
    }

    pub async fn exec(
        &self,
        request: &str,
        data: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, FramaCError> {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id();
        inner
            .send_command(&FramaCCommand::Exec {
                id: id.clone(),
                request: request.to_string(),
                data,
            })
            .await?;
        inner.poll_loop(&id, timeout).await
    }

    pub async fn fetch_all(
        &self,
        request: &str,
    ) -> Result<Vec<serde_json::Value>, FramaCError> {
        let mut all_entries = Vec::new();
        loop {
            let data = self.get(request, serde_json::json!(20000)).await?;
            // Check reload flag before extending (clear stale accumulated entries)
            if data
                .get("reload")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                all_entries.clear();
            }
            if let Some(updated) = data.get("updated").and_then(|v| v.as_array()) {
                all_entries.extend(updated.iter().cloned());
            }
            let pending = data
                .get("pending")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if pending == 0 {
                break;
            }
        }
        Ok(all_entries)
    }

    pub async fn shutdown(&self) -> Result<(), FramaCError> {
        let mut inner = self.inner.lock().await;
        inner.send_command(&FramaCCommand::Shutdown).await?;
        inner.transport.close().await
    }
}
