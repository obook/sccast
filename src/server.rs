use std::net::{IpAddr, TcpListener, UdpSocket};
use std::path::Path;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::task::{Context as TaskContext, Poll};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use axum::body::{BodyDataStream, Bytes};
use futures_core::Stream;

use anyhow::Context;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{HeaderValue, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

/// Local address on the interface that routes to `device` (connect() on UDP sends nothing).
pub fn lan_ip(device: IpAddr) -> std::io::Result<IpAddr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.connect((device, 9))?;
    Ok(socket.local_addr()?.ip())
}

/// Serve `file` at `/media` on `port`. Only that file is exposed, not its directory.
pub fn serve_file(file: &Path, port: u16, stop: &'static AtomicBool) -> anyhow::Result<JoinHandle<()>> {
    spawn(Router::new().route_service("/media", ServeFile::new(file)), port, stop)
}

/// Serve the HLS directory `dir` at the root on `port`.
pub fn serve_hls(dir: &Path, port: u16, stop: &'static AtomicBool) -> anyhow::Result<JoinHandle<()>> {
    // Measures of a previous cast would hide a slow start.
    FETCHES.lock().unwrap().clear();
    let app = Router::new()
        .fallback_service(ServeDir::new(dir))
        .layer(middleware::from_fn(hls_headers));
    spawn(app, port, stop)
}

/// Relay the local Matroska stream on `source` at `/live.mkv` on `port`, one TCP connection per request.
pub fn serve_mkv(source: u16, port: u16, stop: &'static AtomicBool) -> anyhow::Result<JoinHandle<()>> {
    let app = Router::new().route(&format!("/{}", crate::capture::MKV), get(move || relay(source)));
    spawn(app, port, stop)
}

async fn relay(source: u16) -> Response {
    match tokio::net::TcpStream::connect(("127.0.0.1", source)).await {
        Ok(tcp) => ([(CONTENT_TYPE, "video/x-matroska")], Body::from_stream(ReaderStream::new(tcp))).into_response(),
        Err(e) => {
            tracing::warn!("cannot reach the Matroska stream: {e}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// mime_guess maps .m3u8 to audio/x-mpegurl and .ts to video/vnd.dlna.mpeg-tts:
/// set the types HLS players expect, and keep the playlist out of caches.
async fn hls_headers(req: Request, next: Next) -> Response {
    let ext = req.uri().path().rsplit_once('.').map(|(_, ext)| ext.to_owned());
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    match ext.as_deref() {
        Some("m3u8") => {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/vnd.apple.mpegurl"));
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        }
        Some("ts") => {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("video/mp2t"));
            let (parts, body) = res.into_parts();
            let length = parts.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());
            let timed = Timed { inner: body.into_data_stream(), start: Instant::now(), remaining: length };
            return Response::from_parts(parts, Body::from_stream(timed));
        }
        _ => {}
    }
    res
}

/// Time to send the last segments to the TV, newest last.
static FETCHES: Mutex<VecDeque<Duration>> = Mutex::new(VecDeque::new());

/// Average time the TV took to download each of its last 5 segments (1 s of video each).
/// Close to 1 s, the network cannot keep up with the stream.
pub fn segment_fetch_time() -> Option<Duration> {
    let fetches = FETCHES.lock().unwrap();
    let last: Vec<_> = fetches.iter().rev().take(5).collect();
    (!last.is_empty()).then(|| last.iter().copied().sum::<Duration>() / last.len() as u32)
}

/// Segment body that records how long it took to send, once the last byte is out.
struct Timed {
    inner: BodyDataStream,
    start: Instant,
    /// Bytes left to send. hyper stops polling once Content-Length is reached, so the end
    /// of the stream (None) never shows: count the bytes instead.
    remaining: Option<u64>,
}

impl Stream for Timed {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let next = Pin::new(&mut self.inner).poll_next(cx);
        let done = match &next {
            Poll::Ready(Some(Ok(chunk))) => self.remaining.is_some_and(|left| left <= chunk.len() as u64),
            Poll::Ready(None) => self.remaining.is_some(),
            _ => false,
        };
        if let Poll::Ready(Some(Ok(chunk))) = &next {
            self.remaining = self.remaining.map(|left| left.saturating_sub(chunk.len() as u64));
        }
        if done {
            // Record once.
            self.remaining = None;
            let elapsed = self.start.elapsed();
            tracing::info!(ms = elapsed.as_millis() as u64, "segment sent");
            let mut fetches = FETCHES.lock().unwrap();
            fetches.push_back(elapsed);
            if fetches.len() > 10 {
                fetches.pop_front();
            }
        }
        next
    }
}

/// Run `app` on `port` from a background thread until `stop` is set; the thread ends once the port is free.
fn spawn(app: Router, port: u16, stop: &'static AtomicBool) -> anyhow::Result<JoinHandle<()>> {
    // The Default Media Receiver rejects responses without CORS headers.
    let app = app
        .layer(CorsLayer::new().allow_origin(Any))
        .layer(TraceLayer::new_for_http());

    // Bind here so a busy port fails before the cast starts.
    let listener = TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("cannot bind port {port}"))?;
    listener.set_nonblocking(true)?;
    tracing::info!(port, "http server listening");

    Ok(std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|rt| {
                rt.block_on(async {
                    let listener = tokio::net::TcpListener::from_std(listener)?;
                    let stopped = async {
                        while !stop.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    };
                    axum::serve(listener, app).with_graceful_shutdown(stopped).await?;
                    Ok(())
                })
            });
        if let Err(e) = result {
            tracing::error!("http server stopped: {e:#}");
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::atomic::AtomicBool;

    static NEVER: AtomicBool = AtomicBool::new(false);

    fn get(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.to_lowercase()
    }

    #[test]
    fn serves_range_with_cors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("media.bin");
        std::fs::write(&file, b"0123456789").unwrap();
        super::serve_file(&file, 18090, &NEVER).unwrap();

        let response = get(18090, "GET /media HTTP/1.1\r\nHost: x\r\nOrigin: http://cc\r\nRange: bytes=2-4\r\nConnection: close\r\n\r\n");
        assert!(response.starts_with("http/1.1 206"), "{response}");
        assert!(response.contains("access-control-allow-origin: *"), "{response}");
        assert!(response.ends_with("234"), "{response}");
    }

    #[test]
    fn stop_frees_the_port() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let server = super::serve_hls(dir.path(), 18092, &STOP).unwrap();
        STOP.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().unwrap();
        super::serve_hls(dir.path(), 18092, &NEVER).unwrap();
    }

    #[test]
    fn serves_hls_types() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("live.m3u8"), b"#EXTM3U").unwrap();
        std::fs::write(dir.path().join("seg00000.ts"), b"x").unwrap();
        super::serve_hls(dir.path(), 18091, &NEVER).unwrap();

        let playlist = get(18091, "GET /live.m3u8 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(playlist.contains("content-type: application/vnd.apple.mpegurl"), "{playlist}");
        assert!(playlist.contains("cache-control: no-cache"), "{playlist}");
        let segment = get(18091, "GET /seg00000.ts HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert!(segment.contains("content-type: video/mp2t"), "{segment}");
    }
}
