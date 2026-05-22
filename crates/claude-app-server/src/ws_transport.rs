//! WebSocket listener — `--listen ws://HOST:PORT`. Codex parity.
//!
//! Each accepted connection is its own JSON-RPC session: separate
//! `initialize` handshake, separate `MessageProcessor`, separate inbox /
//! outbox. The shared `ThreadStore` + `SidecarClient` are passed in by the
//! main entry point so multiple connections can list/resume the same
//! threads.
//!
//! `Origin` rejection + `/healthz` / `/readyz` health probes match codex.

use crate::outgoing::OutgoingSender;
use crate::processor::MessageProcessor;
use crate::sidecar::SidecarClient;
use crate::thread_store::ThreadStore;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use claude_app_server_protocol::JsonRpcMessage;
use futures_util::{sink::SinkExt, stream::StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

#[derive(Clone)]
struct WsState {
    sidecar: SidecarClient,
    store: ThreadStore,
    default_model: String,
}

pub async fn serve(
    addr: SocketAddr,
    sidecar: SidecarClient,
    store: ThreadStore,
    default_model: String,
) -> anyhow::Result<()> {
    let state = WsState { sidecar, store, default_model };
    let app = Router::new()
        .route("/", get(ws_handler))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state);

    info!("ws transport listening on ws://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz(headers: HeaderMap) -> impl IntoResponse {
    // Match codex behavior: reject any request that carries an Origin
    // header to prevent CSWSH (cross-site websocket hijacking) probes
    // hitting health endpoints.
    if headers.contains_key("origin") {
        return (StatusCode::FORBIDDEN, "forbidden");
    }
    (StatusCode::OK, "ok")
}

async fn readyz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(origin) = headers.get("origin") {
        // Same anti-CSWSH guard. Clients should connect without Origin
        // (Node `ws`, native clients) or only from approved origins. We
        // keep the strict default; future work could read an allow-list.
        warn!(?origin, "ws connection rejected due to Origin header");
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
        .into_response()
}

async fn handle_socket(socket: WebSocket, state: WsState) {
    let (mut sink, mut stream) = socket.split();

    let (out_tx, mut out_rx) = mpsc::channel::<JsonRpcMessage>(1024);
    let outgoing = OutgoingSender::new(out_tx);
    let processor = Arc::new(MessageProcessor::with_store(
        outgoing,
        state.sidecar.clone(),
        state.default_model.clone(),
        state.store.clone(),
    ));

    // Writer task: drain outbound messages into the websocket sink.
    let write_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(text) => {
                    if sink.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("ws encode failed: {e}"),
            }
        }
        let _ = sink.close().await;
    });

    while let Some(frame) = stream.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                debug!("ws stream closed with error: {e}");
                break;
            }
        };
        match frame {
            Message::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() { continue; }
                match serde_json::from_str::<JsonRpcMessage>(trimmed) {
                    Ok(msg) => {
                        let p = processor.clone();
                        p.handle(msg).await;
                    }
                    Err(e) => warn!("ws decode failed: {e}"),
                }
            }
            Message::Binary(_) => {
                warn!("ws binary frames ignored");
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => break,
        }
    }

    // Drop the processor (and thus the outgoing sender) so the writer task
    // observes channel close and shuts down cleanly.
    drop(processor);
    let _ = write_task.await;
    debug!("ws connection closed");
}
