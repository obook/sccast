//! timing.rs
//! Time to send each HLS segment to the TV, shown in the menu's Live panel.
//! It measures the copy into the kernel's send buffer: it only grows on a badly congested
//! network (0 to 1 ms on a quiet one).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, BodyDataStream, Bytes};
use futures_core::Stream;

/// Time to send the last segments to the TV, newest last.
static FETCHES: Mutex<VecDeque<Duration>> = Mutex::new(VecDeque::new());

/// Clear the segment measures: those of a previous cast would hide a slow start.
pub fn forget_fetches() {
    FETCHES.lock().unwrap().clear();
}

/// Average time the TV took to download each of its last 5 segments (1 s of video each).
/// Close to 1 s, the network cannot keep up with the stream.
pub fn segment_fetch_time() -> Option<Duration> {
    let fetches = FETCHES.lock().unwrap();
    let last: Vec<_> = fetches.iter().rev().take(5).collect();
    (!last.is_empty()).then(|| last.iter().copied().sum::<Duration>() / last.len() as u32)
}

/// Wrap a segment body of `length` bytes so its send time is recorded.
pub fn timed(body: Body, length: Option<u64>) -> Body {
    Body::from_stream(Timed { inner: body.into_data_stream(), start: Instant::now(), remaining: length })
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

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
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
