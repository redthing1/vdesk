use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower_http::services::ServeDir;

use crate::state::Secret;

#[derive(Debug, Clone)]
pub struct ViewerConfig {
    pub token: Secret,
    pub rfb_addr: SocketAddr,
    pub novnc_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewerQuery {
    token: String,
}

pub fn router(config: ViewerConfig) -> Router {
    let novnc_dir = config.novnc_dir.clone();
    Router::new()
        .route("/viewer/ws", get(upgrade))
        .nest_service("/novnc", ServeDir::new(novnc_dir).append_index_html_on_directories(true))
        .with_state(config)
}

async fn upgrade(
    State(config): State<ViewerConfig>,
    Query(query): Query<ViewerQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !constant_time_equal(query.token.as_bytes(), config.token.expose().as_bytes()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade.on_upgrade(move |socket| bridge(socket, config.rfb_addr))
}

async fn bridge(socket: WebSocket, rfb_addr: SocketAddr) {
    let Ok(rfb) = TcpStream::connect(rfb_addr).await else {
        return;
    };
    let (mut websocket_write, mut websocket_read) = socket.split();
    let (mut rfb_read, mut rfb_write) = rfb.into_split();

    let browser_to_rfb = async {
        while let Some(message) = websocket_read.next().await {
            match message {
                Ok(Message::Binary(bytes)) => rfb_write.write_all(&bytes).await?,
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Text(_)) => {}
            }
        }
        rfb_write.shutdown().await
    };

    let rfb_to_browser = async {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = rfb_read.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            if websocket_write.send(Message::Binary(buffer[..read].to_vec().into())).await.is_err()
            {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    };

    tokio::select! {
        _ = browser_to_rfb => {}
        _ = rfb_to_browser => {}
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_comparison_checks_length_and_contents() {
        assert!(constant_time_equal(b"abcdefghijklmnop", b"abcdefghijklmnop"));
        assert!(!constant_time_equal(b"abcdefghijklmnop", b"abcdefghijklmnoq"));
        assert!(!constant_time_equal(b"short", b"longer"));
    }
}
