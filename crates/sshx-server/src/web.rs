//! HTTP and WebSocket handlers for the sshx web interface.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::Path;
use axum::response::IntoResponse;
use axum::routing::{get, get_service};
use axum::{Extension, Router};
use hyper::StatusCode;
use serde::{Deserialize, Serialize};
use sshx_core::proto::{server_update::ServerMessage, TerminalInput, TerminalSize};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info_span, warn, Instrument};

use crate::session::Session;
use crate::state::ServerState;

/// Returns the web application server, built with Axum.
pub fn app(state: Arc<ServerState>) -> Router {
    Router::new()
        .nest("/api", backend(state))
        .fallback(frontend())
}

/// Serves static SvelteKit build files.
fn frontend() -> Router {
    let root_spa = ServeFile::new("build/spa.html")
        .precompressed_gzip()
        .precompressed_br();

    let static_files = ServeDir::new("build")
        .precompressed_gzip()
        .precompressed_br()
        .fallback(root_spa);

    Router::new().nest("/", get_service(static_files).handle_error(error_handler))
}

/// Error handler for tower-http services.
async fn error_handler(error: io::Error) -> impl IntoResponse {
    let message = format!("unhandled internal error: {error}");
    error!("{message}");
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// Runs the backend web API server.
fn backend(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/s/:id", get(get_session_ws))
        .layer(Extension(state))
}

/// Real-time message conveying the position and size of a terminal.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WsWinsize {
    /// The top-left x-coordinate of the window, offset from origin.
    pub x: i32,
    /// The top-left y-coordinate of the window, offset from origin.
    pub y: i32,
    /// The number of rows in the window.
    pub rows: u16,
    /// The number of columns in the terminal.
    pub cols: u16,
}

impl Default for WsWinsize {
    fn default() -> Self {
        WsWinsize {
            x: 0,
            y: 0,
            rows: 24,
            cols: 80,
        }
    }
}

/// A real-time message sent from the server over WebSocket.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum WsServer {
    /// Notification when the set of open shells has changed.
    Shells(Vec<(u32, WsWinsize)>),
    /// Subscription results, chunks of terminal data.
    Chunks(u32, Vec<(u64, String)>),
    /// The current session has been terminated.
    Terminated(),
    /// Send an error message to the client.
    Error(String),
}

/// A real-time message sent from the client over WebSocket.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum WsClient {
    /// Create a new shell.
    Create(),
    /// Close a specific shell.
    Close(u32),
    /// Move a shell window to a new position and focus it.
    Move(u32, Option<WsWinsize>),
    /// Add user data to a given shell.
    Data(u32, #[serde(with = "serde_bytes")] Vec<u8>),
    /// Subscribe to a shell, starting at a given chunk index.
    Subscribe(u32, u64),
}

async fn get_session_ws(
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<ServerState>>,
) -> impl IntoResponse {
    if let Some(session) = state.store.get(&id) {
        let session = Arc::clone(&*session);
        ws.on_upgrade(move |socket| {
            async {
                if let Err(err) = handle_socket(socket, session).await {
                    warn!(?err, "exiting early");
                }
            }
            .instrument(info_span!("ws", %id))
        })
    } else {
        ws.on_upgrade(|mut socket| async move {
            let frame = CloseFrame {
                code: 4404,
                reason: "could not find the requested session".into(),
            };
            socket.send(Message::Close(Some(frame))).await.ok();
        })
    }
}

/// Handle an incoming live WebSocket connection to a given session.
async fn handle_socket(mut socket: WebSocket, session: Arc<Session>) -> Result<()> {
    /// Send a message to the client over WebSocket.
    async fn send(socket: &mut WebSocket, msg: WsServer) -> Result<()> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&msg, &mut buf)?;
        socket.send(Message::Binary(buf)).await?;
        Ok(())
    }

    /// Receive a message from the client over WebSocket.
    async fn recv(socket: &mut WebSocket) -> Result<Option<WsClient>> {
        Ok(loop {
            match socket.recv().await.transpose()? {
                Some(Message::Text(_)) => warn!("ignoring text message over WebSocket"),
                Some(Message::Binary(msg)) => break Some(ciborium::de::from_reader(&msg[..])?),
                Some(_) => (), // ignore other message types, keep looping
                None => break None,
            }
        })
    }

    let mut subscribed = HashSet::new(); // prevent duplicate subscriptions
    let (chunks_tx, mut chunks_rx) = mpsc::channel::<(u32, Vec<(u64, String)>)>(1);

    let update_tx = session.update_tx();
    let shells_stream = session.subscribe_shells();
    tokio::pin!(shells_stream);
    loop {
        let msg = tokio::select! {
            _ = session.terminated() => {
                send(&mut socket, WsServer::Terminated()).await?;
                socket.close().await?;
                break;
            }
            Some(shells) = shells_stream.next() => {
                send(&mut socket, WsServer::Shells(shells)).await?;
                continue;
            }
            Some((id, chunks)) = chunks_rx.recv() => {
                send(&mut socket, WsServer::Chunks(id, chunks)).await?;
                continue;
            }
            result = recv(&mut socket) => {
                match result? {
                    Some(msg) => msg,
                    None => break,
                }
            }
        };

        match msg {
            WsClient::Create() => {
                let id = session.next_id();
                update_tx.send(ServerMessage::CreateShell(id)).await?;
            }
            WsClient::Close(id) => {
                update_tx.send(ServerMessage::CloseShell(id)).await?;
            }
            WsClient::Move(id, winsize) => {
                if let Err(err) = session.move_shell(id, winsize) {
                    send(&mut socket, WsServer::Error(err.to_string())).await?;
                    continue;
                }
                if let Some(winsize) = winsize {
                    let msg = ServerMessage::Resize(TerminalSize {
                        id,
                        rows: winsize.rows as u32,
                        cols: winsize.cols as u32,
                    });
                    session.update_tx().send(msg).await?;
                }
            }
            WsClient::Data(id, data) => {
                let data = TerminalInput { id, data };
                update_tx.send(ServerMessage::Input(data)).await?;
            }
            WsClient::Subscribe(id, chunknum) => {
                if subscribed.contains(&id) {
                    continue;
                }
                subscribed.insert(id);
                let session = Arc::clone(&session);
                let chunks_tx = chunks_tx.clone();
                tokio::spawn(async move {
                    let stream = session.subscribe_chunks(id, chunknum);
                    tokio::pin!(stream);
                    while let Some(chunks) = stream.next().await {
                        if chunks_tx.send((id, chunks)).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }
    }
    Ok(())
}
