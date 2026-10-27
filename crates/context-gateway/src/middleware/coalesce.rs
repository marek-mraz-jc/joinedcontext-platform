//! One frame per [`CAP`] bytes for the compression layer (T-5851).
//!
//! async-compression ends a deflate or brotli block whenever the body it reads answers `Pending`
//! after it has taken bytes, so the client sees a partial answer early. A proxied answer arrives
//! as hyper's network reads with a `Pending` between them: the 421 KB bikes page left the gateway
//! as 35 KB of br where the whole body packs to 20 KB. [`layer`] holds the frames of an answer
//! until [`CAP`] bytes or its end and hands them on as one, so the encoder sees few `Pending`s.
//! An event stream passes untouched: each event must leave when it is written.

use axum::body::{Body, Bytes, HttpBody};
use axum::http::{header, Response};
use http_body::{Frame, SizeHint};
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::util::MapResponseLayer;

/// The most one answer holds. 256 KiB keeps a block of brotli quality 2 or gzip level 1 within a
/// few percent of the whole body's size, and bounds what an answer the gateway can already read
/// whole costs per request; a bigger answer still streams, one frame per 256 KiB.
pub const CAP: usize = 256 * 1024;

/// What [`layer`] maps every answer with.
pub type Coalescing = MapResponseLayer<fn(Response<Body>) -> Response<Body>>;

/// The layer that coalesces every answer but an event stream; it goes inside the compression
/// layers.
pub fn layer() -> Coalescing {
    MapResponseLayer::new(coalesce as fn(Response<Body>) -> Response<Body>)
}

fn coalesce(response: Response<Body>) -> Response<Body> {
    let event_stream = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().starts_with("text/event-stream"));
    if event_stream {
        return response;
    }
    response.map(|inner| {
        Body::new(Coalesce {
            inner,
            held: Vec::new(),
            trailers: None,
            ended: false,
        })
    })
}

struct Coalesce {
    inner: Body,
    /// Data read since the last frame went out, never more than [`CAP`] plus one inner frame.
    held: Vec<u8>,
    /// A trailers frame, sent after the data held before it.
    trailers: Option<Frame<Bytes>>,
    ended: bool,
}

impl Coalesce {
    fn release(&mut self) -> Frame<Bytes> {
        Frame::data(Bytes::from(std::mem::take(&mut self.held)))
    }
}

impl HttpBody for Coalesce {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = &mut *self;
        loop {
            if this.ended || this.trailers.is_some() {
                if !this.held.is_empty() {
                    return Poll::Ready(Some(Ok(this.release())));
                }
                return Poll::Ready(this.trailers.take().map(Ok));
            }
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => this.ended = true,
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    // A frame of a cap or more leaves as it is, uncopied.
                    Ok(data) if this.held.is_empty() && data.len() >= CAP => {
                        return Poll::Ready(Some(Ok(Frame::data(data))));
                    }
                    Ok(data) => {
                        this.held.extend_from_slice(&data);
                        if this.held.len() >= CAP {
                            return Poll::Ready(Some(Ok(this.release())));
                        }
                    }
                    Err(trailers) => this.trailers = Some(trailers),
                },
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.held.is_empty() && self.trailers.is_none() && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = self.inner.size_hint();
        let held = self.held.len() as u64;
        hint.set_lower(hint.lower().saturating_add(held));
        if let Some(upper) = hint.upper() {
            hint.set_upper(upper.saturating_add(held));
        }
        hint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    /// `chunks` as a body whose every frame comes after one `Pending`.
    fn pending_between(chunks: Vec<&'static [u8]>) -> Body {
        use futures_util::StreamExt;
        Body::from_stream(
            futures_util::stream::unfold(chunks.into_iter(), |mut chunks| async move {
                tokio::task::yield_now().await;
                chunks
                    .next()
                    .map(|c| (Ok::<_, std::io::Error>(Bytes::from_static(c)), chunks))
            })
            .fuse(),
        )
    }

    fn json(body: Body) -> Response<Body> {
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .expect("a response")
    }

    async fn frames(response: Response<Body>) -> Vec<Vec<u8>> {
        let mut body = coalesce(response).into_body();
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            out.push(frame.expect("a frame").into_data().expect("data").to_vec());
        }
        out
    }

    #[tokio::test]
    async fn small_frames_leave_as_one() {
        let got = frames(json(pending_between(vec![b"[1,", b"2,", b"3]"]))).await;
        assert_eq!(got, vec![b"[1,2,3]".to_vec()]);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_leaves_one_frame_per_cap() {
        static HALF: [u8; CAP / 2] = [b'a'; CAP / 2];
        let got = frames(json(pending_between(vec![&HALF, &HALF, &HALF]))).await;
        assert_eq!(
            got.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![CAP, CAP / 2]
        );
    }

    #[tokio::test]
    async fn an_event_stream_keeps_its_frames() {
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(pending_between(vec![b"data: 1\n\n", b"data: 2\n\n"]))
            .expect("a response");
        let got = frames(response).await;
        assert_eq!(got, vec![b"data: 1\n\n".to_vec(), b"data: 2\n\n".to_vec()]);
    }

    #[tokio::test]
    async fn the_size_of_a_full_body_stays_exact() {
        let body = coalesce(json(Body::from("[1,2,3]"))).into_body();
        assert_eq!(body.size_hint().exact(), Some(7));
    }

    #[tokio::test]
    async fn trailers_follow_the_data_held_before_them() {
        let mut map = axum::http::HeaderMap::new();
        map.insert("grpc-status", "0".parse().expect("a value"));
        let frames_in = vec![
            Ok::<_, std::io::Error>(Frame::data(Bytes::from_static(b"ab"))),
            Ok(Frame::data(Bytes::from_static(b"cd"))),
            Ok(Frame::trailers(map.clone())),
        ];
        let inner = Body::new(http_body_util::StreamBody::new(futures_util::stream::iter(
            frames_in,
        )));
        let mut body = coalesce(json(inner)).into_body();
        let data = body.frame().await.expect("a frame").expect("ok");
        assert_eq!(data.into_data().expect("data"), Bytes::from_static(b"abcd"));
        let trailers = body.frame().await.expect("a frame").expect("ok");
        assert_eq!(trailers.into_trailers().expect("trailers"), map);
        assert!(body.frame().await.is_none());
    }
}
