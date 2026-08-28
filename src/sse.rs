//! Latency is seconds, so the board is pushed rather than polled.

use crate::events::BoardEvent;
use crate::store::{CatchUpResult, SharedBoard};

use axum::extract::{Query, State as AxState};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as TokioStreamExt;

#[derive(Debug, Deserialize)]
pub struct EventParams {
    pub last_seq: Option<u64>,
}

pub async fn events(
    AxState(b): AxState<SharedBoard>,
    Query(params): Query<EventParams>,
    headers: HeaderMap,
) -> Response {
    // Emit a comment frame immediately so proxies (Vite in particular) flush
    // response headers and the browser's EventSource fires `onopen` without
    // waiting for the first keep-alive (~15s) or a real board event.
    let hello = stream::once(async {
        Ok::<_, Infallible>(Event::default().comment("connected"))
    });

    let last_seq = params.last_seq.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    });

    let live_rx = BroadcastStream::new(b.subscribe());

    let (initial_events, min_live_seq) = match last_seq {
        Some(seq) => match b.catch_up(seq) {
            CatchUpResult::Events(missed) => {
                let max_seq = missed.last().map(|e| e.seq()).unwrap_or(seq);
                (missed, max_seq)
            }
            CatchUpResult::Reset { seq: current_seq } => {
                (vec![BoardEvent::Reset { seq: current_seq }], current_seq)
            }
        },
        None => (vec![], 0),
    };

    let catchup_stream = stream::iter(initial_events.into_iter().filter_map(|ev| {
        Event::default().json_data(&ev).ok().map(Ok::<_, Infallible>)
    }));

    let b_lag = b.clone();
    let live_stream = TokioStreamExt::filter_map(live_rx, move |msg| {
        match msg {
            Ok(ev) => {
                if ev.seq() > min_live_seq {
                    Event::default().json_data(&ev).ok().map(Ok)
                } else {
                    None
                }
            }
            Err(_lagged) => {
                let reset_ev = BoardEvent::Reset {
                    seq: b_lag.current_seq(),
                };
                Event::default().json_data(&reset_ev).ok().map(Ok)
            }
        }
    });

    let sse = Sse::new(StreamExt::chain(
        hello,
        StreamExt::chain(catchup_stream, live_stream),
    ))
    .keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    );

    let mut res = sse.into_response();
    // Hint reverse proxies not to buffer the event stream.
    res.headers_mut().insert(
        header::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Origin;
    use crate::schema::Schema;
    use crate::store::Board;
    use axum::extract::{Query, State as AxState};
    use axum::http::HeaderMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_sse_endpoint_catchup_and_reset() {
        let b = Arc::new(
            Board::new(
                Schema::default(),
                std::env::temp_dir().join(format!("sandboard-sse-test-{}.json", std::process::id())),
            )
            .with_buffer_capacity(2),
        );

        let _p = b
            .create(None, "SSE Proj", "intent", None, Origin::Human, true, None)
            .unwrap();
        let cur_seq = b.current_seq();

        let resp = events(
            AxState(b.clone()),
            Query(EventParams {
                last_seq: Some(cur_seq - 1),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), 200);

        let resp_lagged = events(
            AxState(b),
            Query(EventParams {
                last_seq: Some(0),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp_lagged.status(), 200);
    }
}
