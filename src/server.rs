use crate::capture::{ClientDiagnostics, EncodedPacket};
use crate::input::{TouchAction, VirtualTouchscreen};
use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, watch, Semaphore};

const CLIENT_HTML: &str = include_str!("client.html");
const MAX_IN_FLIGHT_FRAMES: usize = 6;

#[derive(Clone)]
pub struct AppState {
    pub packet_tx: broadcast::Sender<EncodedPacket>,
    pub latest_keyframe: Arc<RwLock<Option<Arc<Vec<u8>>>>>,
    pub diagnostics: Arc<ClientDiagnostics>,
    pub touch_dev: Arc<Mutex<VirtualTouchscreen>>,
    pub html_page: Arc<String>,
}

pub async fn run_http_ws_server(
    port: u16,
    is_landscape: bool,
    packet_tx: broadcast::Sender<EncodedPacket>,
    latest_keyframe: Arc<RwLock<Option<Arc<Vec<u8>>>>>,
    diagnostics: Arc<ClientDiagnostics>,
    touch_dev: Arc<Mutex<VirtualTouchscreen>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let orient_str = if is_landscape { "landscape" } else { "portrait" };
    let html_page = Arc::new(CLIENT_HTML.replace("__TARGET_ORIENTATION__", orient_str));

    let state = AppState {
        packet_tx,
        latest_keyframe,
        diagnostics,
        touch_dev,
        html_page,
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    use axum::serve::ListenerExt;
    let listener = tokio::net::TcpListener::bind(addr)
        .await?
        .tap_io(|tcp_stream| {
            let _ = tcp_stream.set_nodelay(true);
        });

    // TCP_NODELAY eliminates 40ms Nagle + Delayed-ACK stalls on WebSocket ACKs & Touch events!
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await?;

    Ok(())
}

async fn index_handler(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store, no-cache, must-revalidate")],
        Html((*state.html_page).clone()),
    )
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut packet_rx = state.packet_tx.subscribe();
    let latest_keyframe = state.latest_keyframe.clone();
    let diag_send = state.diagnostics.clone();
    let diag_recv = state.diagnostics.clone();
    let touch_dev = state.touch_dev.clone();

    let in_flight_sem = Arc::new(Semaphore::new(MAX_IN_FLIGHT_FRAMES));
    let in_flight_ack = in_flight_sem.clone();

    let send_task = tokio::spawn(async move {
        // Send the initial full keyframe immediately upon connection
        let initial_kf = latest_keyframe.read().ok().and_then(|g| g.clone());
        if let Some(kf) = initial_kf {
            if let Ok(permit) = in_flight_sem.acquire().await {
                permit.forget();
                if sender
                    .send(Message::Binary(kf.as_ref().clone().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        loop {
            let Ok(permit) = in_flight_sem.acquire().await else {
                break;
            };
            permit.forget();

            match packet_rx.recv().await {
                Ok(pkt) => {
                    if sender
                        .send(Message::Binary(pkt.data.as_ref().clone().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    diag_send.lagged_events.fetch_add(1, Ordering::Relaxed);
                    // Self-heal by sending the latest full keyframe if delta packets were dropped
                    let kf_opt = latest_keyframe.read().ok().and_then(|g| g.clone());
                    if let Some(kf) = kf_opt {
                        if sender
                            .send(Message::Binary(kf.as_ref().clone().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    } else if in_flight_sem.available_permits() < MAX_IN_FLIGHT_FRAMES {
                        in_flight_sem.add_permits(1);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Binary(data) = msg {
            if !data.is_empty() && data[0] == 0xFF {
                diag_recv.acks_received.fetch_add(1, Ordering::Relaxed);
                if data.len() >= 4 {
                    diag_recv.decode_ms.store(data[1] as u32, Ordering::Relaxed);
                    diag_recv.client_fps.store(data[2] as u32, Ordering::Relaxed);
                    diag_recv.queue_len.store(data[3] as u32, Ordering::Relaxed);
                }
                if in_flight_ack.available_permits() < MAX_IN_FLIGHT_FRAMES {
                    in_flight_ack.add_permits(1);
                }
            } else if data.len() >= 6 {
                let action_byte = data[0];
                let slot = data[1];
                let x = u16::from_le_bytes([data[2], data[3]]);
                let y = u16::from_le_bytes([data[4], data[5]]);
                if let Some(action) = TouchAction::from_u8(action_byte) {
                    if let Ok(mut dev) = touch_dev.lock() {
                        let _ = dev.handle_touch_event(action, slot, x, y);
                    }
                }
            }
        }
    }

    send_task.abort();
}
