//! `GET /v1/live`: Server-Sent Events with the user's Logged Hours totals.
//! The first event is the current totals; later ones come from the fanout.
//! A `resync` event follows a missed update, then fresh totals.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::{self, Stream};
use serde_json::json;
use tokio::sync::broadcast::Receiver;

use crate::app::{AppState, authenticate};
use crate::error::ApiError;
use crate::fanout::{LiveEvent, Totals, next_event};
use crate::merge::user_totals;

fn totals_event(totals: Totals) -> Event {
    Event::default().event("totals").data(
        json!({
            "human_active_sec": totals.human_active_sec,
            "struggle_credit_sec": totals.struggle_credit_sec,
            "agent_compute_sec": totals.agent_compute_sec,
        })
        .to_string(),
    )
}

struct Stream_ {
    state: Arc<AppState>,
    user: String,
    receiver: Receiver<LiveEvent>,
    queued: Vec<Event>,
    started: bool,
}

pub async fn stream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let device = authenticate(&state, &headers).await?;
    // Subscribe before reading the totals so no update falls in the gap.
    let receiver = state.fanout.subscribe(&device.user_id);
    let initial = Stream_ {
        state,
        user: device.user_id,
        receiver,
        queued: Vec::new(),
        started: false,
    };
    let events = stream::unfold(initial, |mut s| async move {
        if !s.started {
            s.started = true;
            let totals = user_totals(&s.state, &s.user).await.unwrap_or_default();
            return Some((Ok(totals_event(totals)), s));
        }
        if let Some(event) = s.queued.pop() {
            return Some((Ok(event), s));
        }
        match next_event(&mut s.receiver).await? {
            LiveEvent::Totals(totals) => Some((Ok(totals_event(totals)), s)),
            LiveEvent::Resync => {
                let totals = user_totals(&s.state, &s.user).await.unwrap_or_default();
                s.queued.push(totals_event(totals));
                Some((Ok(Event::default().event("resync").data("{}")), s))
            }
        }
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}
