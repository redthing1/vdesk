use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
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
use tokio::process::Command;

use crate::state::Secret;

const VIEWER_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; \
                          connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'none'";

#[derive(Debug, Clone)]
pub struct ViewerConfig {
    pub token: Secret,
    pub target: RfbTarget,
}

#[derive(Debug, Clone)]
pub enum RfbTarget {
    Tcp(SocketAddr),
    Command(Arc<[String]>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewerQuery {
    token: String,
}

pub fn router(config: ViewerConfig) -> Router {
    Router::new()
        .route("/viewer/ws", get(upgrade))
        .route("/novnc/vnc.html", get(viewer_html))
        .route("/novnc/viewer.js", get(viewer_script))
        .route("/novnc/rfb.js", get(novnc_script))
        .with_state(config)
}

async fn viewer_html() -> Response {
    static_asset("text/html; charset=utf-8", include_bytes!("../assets/novnc/vnc.html"))
}

async fn viewer_script() -> Response {
    static_asset("text/javascript; charset=utf-8", include_bytes!("../assets/novnc/viewer.js"))
}

async fn novnc_script() -> Response {
    static_asset("text/javascript; charset=utf-8", include_bytes!("../assets/novnc/rfb.js"))
}

fn static_asset(content_type: &'static str, bytes: &'static [u8]) -> Response {
    Response::builder()
        .header("content-type", content_type)
        .header("cache-control", "private, max-age=86400")
        .header("content-security-policy", VIEWER_CSP)
        .header("referrer-policy", "no-referrer")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(bytes))
        .expect("static viewer response is valid")
}

async fn upgrade(
    State(config): State<ViewerConfig>,
    Query(query): Query<ViewerQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !constant_time_equal(query.token.as_bytes(), config.token.expose().as_bytes()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade.on_upgrade(move |socket| bridge(socket, config.target))
}

async fn bridge(socket: WebSocket, target: RfbTarget) {
    match target {
        RfbTarget::Tcp(address) => {
            let Ok(rfb) = TcpStream::connect(address).await else {
                return;
            };
            let (read, write) = rfb.into_split();
            bridge_stream(socket, read, write).await;
        }
        RfbTarget::Command(argv) => bridge_command(socket, &argv).await,
    }
}

async fn bridge_command(socket: WebSocket, argv: &[String]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    else {
        return;
    };
    let (Some(read), Some(write)) = (child.stdout.take(), child.stdin.take()) else {
        let _ = child.kill().await;
        return;
    };
    bridge_stream(socket, read, write).await;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn bridge_stream(
    socket: WebSocket,
    mut rfb_read: impl tokio::io::AsyncRead + Unpin,
    mut rfb_write: impl tokio::io::AsyncWrite + Unpin,
) {
    let (mut websocket_write, mut websocket_read) = socket.split();

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
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn credential_comparison_checks_length_and_contents() {
        assert!(constant_time_equal(b"abcdefghijklmnop", b"abcdefghijklmnop"));
        assert!(!constant_time_equal(b"abcdefghijklmnop", b"abcdefghijklmnoq"));
        assert!(!constant_time_equal(b"short", b"longer"));
    }

    #[tokio::test]
    async fn viewer_assets_are_built_in() {
        let app = router(ViewerConfig {
            token: Secret::parse("viewer-token-123456".into()).unwrap(),
            target: RfbTarget::Command(Arc::from([])),
        });
        let response = app
            .oneshot(Request::builder().uri("/novnc/vnc.html").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-security-policy"], VIEWER_CSP);
        let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        assert!(body.windows(b"viewer.js".len()).any(|window| window == b"viewer.js"));
    }
}
