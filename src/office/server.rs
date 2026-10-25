//! The office server: the page, the roster, and the two sockets.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc};

use super::auth;
use super::protocol::{
    AgentId, AgentInfo, FromAgent, FromBrowser, ToAgent, ToBrowser, TurnEvent, TurnId,
};
use super::registry::{self, Registration};
use crate::command::{self, Input};

/// Events kept per agent so a browser arriving late can catch up.
const TAIL_CAP: usize = 200;
/// How long a departed agent stays visible.
const FORGET_AFTER: Duration = Duration::from_secs(60);
/// Browser broadcasts may lag; a stale frame is cheaper than a stalled room.
const BROADCAST_CAPACITY: usize = 256;
/// Prompts waiting for one agent socket.
const OUTBOX_CAPACITY: usize = 64;

const PAGE: &str = "<!doctype html><meta charset=\"utf-8\">\
<title>Jarvo office</title><h1>Jarvo office</h1>\
<p>The office page arrives in a later task.</p>";

/// Shared state: the signing key, the room, and the fan-out to browsers.
#[derive(Clone)]
struct Office {
    key: Arc<Vec<u8>>,
    room: Arc<Mutex<Room>>,
    browsers: broadcast::Sender<ToBrowser>,
}

impl Office {
    fn new(key: Vec<u8>) -> Self {
        let (browsers, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            key: Arc::new(key),
            room: Arc::new(Mutex::new(Room::new())),
            browsers,
        }
    }

    async fn roster(&self) -> Vec<AgentInfo> {
        self.room.lock().await.roster()
    }

    fn broadcast(&self, message: ToBrowser) {
        // A send error only means no browser is listening.
        let _ = self.browsers.send(message);
    }

    async fn join(&self, mut info: AgentInfo, outbox: mpsc::Sender<ToAgent>) -> AgentInfo {
        let mut room = self.room.lock().await;
        info.name = unique_name(room.names(&info.id.0), &info.name);
        room.add(info.clone(), outbox);
        drop(room);
        self.broadcast(ToBrowser::Joined { info: info.clone() });
        info
    }

    async fn publish_turn(&self, id: &AgentId, turn: TurnId, event: TurnEvent) {
        push_tail(&self.room, id, turn.clone(), event.clone()).await;
        self.broadcast(ToBrowser::Turn {
            id: id.clone(),
            turn,
            event,
        });
    }

    /// Mark the agent disconnected. `true` means this close still owes a `Left`.
    async fn agent_gone(&self, id: &AgentId) -> bool {
        self.room.lock().await.gone(&id.0)
    }
}

#[derive(Default)]
struct Room {
    agents: HashMap<String, Agent>,
}

struct Agent {
    info: AgentInfo,
    tail: VecDeque<(TurnId, TurnEvent)>,
    outbox: mpsc::Sender<ToAgent>,
    connected: bool,
}

impl Room {
    fn new() -> Self {
        Self::default()
    }

    /// Insert or reconnect an agent, preserving the tail either way.
    fn add(&mut self, info: AgentInfo, outbox: mpsc::Sender<ToAgent>) {
        let tail = self
            .agents
            .remove(&info.id.0)
            .map(|agent| agent.tail)
            .unwrap_or_default();
        self.agents.insert(
            info.id.0.clone(),
            Agent {
                info,
                tail,
                outbox,
                connected: true,
            },
        );
    }

    fn names<'a>(&'a self, except: &'a str) -> impl Iterator<Item = &'a str> {
        self.agents
            .values()
            .filter(move |agent| agent.info.id.0 != except)
            .map(|agent| agent.info.name.as_str())
    }

    fn roster(&self) -> Vec<AgentInfo> {
        self.agents
            .values()
            .map(|agent| agent.info.clone())
            .collect()
    }

    /// `true` only on the first close, so a `Left` goes out exactly once.
    fn gone(&mut self, id: &str) -> bool {
        match self.agents.get_mut(id) {
            Some(agent) if agent.connected => {
                agent.connected = false;
                true
            }
            _ => false,
        }
    }
}

/// Append a served or stored event, dropping the oldest beyond the cap.
async fn push_tail(room: &Mutex<Room>, id: &AgentId, turn: TurnId, event: TurnEvent) {
    let mut room = room.lock().await;
    let Some(agent) = room.agents.get_mut(&id.0) else {
        return;
    };
    agent.tail.push_back((turn, event));
    while agent.tail.len() > TAIL_CAP {
        agent.tail.pop_front();
    }
}

/// Forget a departed agent unless its id has reconnected in the meantime.
async fn forget_after(room: Arc<Mutex<Room>>, id: AgentId, delay: Duration) {
    tokio::time::sleep(delay).await;
    let mut room = room.lock().await;
    if room.agents.get(&id.0).is_some_and(|agent| !agent.connected) {
        room.agents.remove(&id.0);
    }
}

/// The display name with a `-2`, `-3`, ... suffix if it is already taken.
fn unique_name<'a>(taken: impl IntoIterator<Item = &'a str>, base: &str) -> String {
    let taken: Vec<&str> = taken.into_iter().collect();
    if !taken.contains(&base) {
        return base.to_string();
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !taken.contains(&candidate.as_str()) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Handle one browser frame without sockets, so the room rules stay testable.
async fn handle_browser_message(room: &Mutex<Room>, message: FromBrowser) -> Option<ToBrowser> {
    match message {
        FromBrowser::Select { id } => {
            let room = room.lock().await;
            let events = room
                .agents
                .get(&id.0)
                .map(|agent| agent.tail.iter().cloned().collect())
                .unwrap_or_default();
            Some(ToBrowser::Tail { id, events })
        }
        FromBrowser::Send { id, text } => {
            let (outbox, skills) = {
                let room = room.lock().await;
                let Some(agent) = room.agents.get(&id.0) else {
                    return Some(refused(id, "no such agent"));
                };
                (agent.outbox.clone(), agent.info.skills.clone())
            };
            let to_agent = match command::parse(&text, &skills) {
                Input::Prompt(text) => ToAgent::Prompt { text },
                Input::Structured(text) => ToAgent::Structured { text },
                Input::Skill { name, request } => ToAgent::Skill { name, request },
                Input::JsonWithoutAPrompt => {
                    return Some(refused(id, "a structured prompt needs text"));
                }
                Input::Clear => {
                    return Some(refused(id, "clearing a conversation is local to the agent"));
                }
                Input::Quit => return Some(refused(id, "quitting is local to the agent")),
            };
            match outbox.send(to_agent).await {
                Ok(()) => None,
                Err(_) => Some(refused(id, "the agent is not connected")),
            }
        }
        // Task 10 replaces this with a real spawn. Until then, say so.
        FromBrowser::Spawn { .. } => Some(refused(
            AgentId(String::new()),
            "spawning is not available yet",
        )),
    }
}

fn refused(id: AgentId, reason: &str) -> ToBrowser {
    ToBrowser::Refused {
        id,
        reason: reason.to_string(),
    }
}

fn router(state: Office) -> Router {
    let http = Router::new()
        .route("/api/agents", get(agents_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    let sockets = Router::new()
        .route("/ws/agent", get(agent_handler))
        .route("/ws/browser", get(browser_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_query_token,
        ));
    Router::new()
        .route("/", get(page_handler))
        .merge(http)
        .merge(sockets)
        .with_state(state)
}

async fn page_handler() -> Html<&'static str> {
    Html(PAGE)
}

async fn agents_handler(State(state): State<Office>) -> Json<Vec<AgentInfo>> {
    Json(state.roster().await)
}

async fn agent_handler(ws: WebSocketUpgrade, State(state): State<Office>) -> Response {
    ws.on_upgrade(move |socket| agent_socket(socket, state))
}

async fn browser_handler(ws: WebSocketUpgrade, State(state): State<Office>) -> Response {
    ws.on_upgrade(move |socket| browser_socket(socket, state))
}

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: String,
}

/// `Authorization: Bearer <jwt>` for plain HTTP.
async fn require_bearer(
    State(state): State<Office>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    match bearer(&headers) {
        Some(token) if authorized(&state, token) => next.run(request).await,
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// `?token=<jwt>` for WebSocket handshakes, which browsers cannot give headers.
async fn require_query_token(
    State(state): State<Office>,
    Query(query): Query<TokenQuery>,
    request: Request,
    next: Next,
) -> Response {
    if authorized(&state, &query.token) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

fn authorized(state: &Office, token: &str) -> bool {
    !token.is_empty() && auth::verify(state.key.as_slice(), token).is_ok()
}

async fn send_json<T: Serialize>(socket: &mut WebSocket, message: &T) -> bool {
    let Ok(text) = serde_json::to_string(message) else {
        return false;
    };
    socket.send(Message::Text(text.into())).await.is_ok()
}

async fn browser_socket(mut socket: WebSocket, state: Office) {
    let mut events = state.browsers.subscribe();
    let roster = state.roster().await;
    if !send_json(&mut socket, &ToBrowser::Roster { agents: roster }).await {
        return;
    }

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => {
                    if !send_json(&mut socket, &event).await {
                        return;
                    }
                }
                // A slow browser loses frames rather than the room.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            incoming = socket.recv() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return,
                };
                let Ok(message) = serde_json::from_str::<FromBrowser>(text.as_str()) else {
                    continue;
                };
                if let Some(reply) = handle_browser_message(&state.room, message).await
                    && !send_json(&mut socket, &reply).await
                {
                    return;
                }
            }
        }
    }
}

async fn agent_socket(mut socket: WebSocket, state: Office) {
    let (outbox, mut inbox) = mpsc::channel(OUTBOX_CAPACITY);
    // Keeps `inbox` open until the socket closes, before and after `Hello`.
    let _keepalive = outbox.clone();
    let mut id: Option<AgentId> = None;

    loop {
        tokio::select! {
            outgoing = inbox.recv() => {
                let Some(message) = outgoing else { break };
                if !send_json(&mut socket, &message).await {
                    break;
                }
            }
            incoming = socket.recv() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => break,
                };
                match serde_json::from_str::<FromAgent>(text.as_str()) {
                    Ok(FromAgent::Hello { info }) => {
                        let info = state.join(info, outbox.clone()).await;
                        id = Some(info.id);
                    }
                    Ok(FromAgent::Turn { turn, event }) => {
                        if let Some(id) = &id {
                            state.publish_turn(id, turn, event).await;
                        }
                    }
                    Err(_) => continue,
                }
            }
        }
    }

    if let Some(id) = id
        && state.agent_gone(&id).await
    {
        state.broadcast(ToBrowser::Left { id: id.clone() });
        tokio::spawn(forget_after(state.room.clone(), id, FORGET_AFTER));
    }
}

/// Run the office server until it stops.
///
/// This is the only place the process prints: it is its own process, and the
/// URL carries the token, so it is printed rather than logged.
pub async fn serve(dir: &Path, bind: &str) -> Result<()> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("`{bind}` is not a socket address"))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the office to `{bind}`"))?;
    let bound = listener
        .local_addr()
        .context("cannot read the office address")?;

    let key = auth::load_or_create_key(dir)
        .with_context(|| format!("cannot load the office key in {}", dir.display()))?;
    let token = auth::issue(&key, "office").context("cannot issue the office token")?;
    let state = Office::new(key);

    let registration = Registration {
        port: bound.port(),
        pid: std::process::id(),
        token: token.clone(),
    };
    registry::publish(dir, &registration)
        .with_context(|| format!("cannot write the office registration in {}", dir.display()))?;

    println!("Jarvo office: http://{bound}/?token={token}");
    axum::serve(listener, router(state))
        .await
        .context("the office server stopped")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn agent(id: &str, name: &str) -> AgentInfo {
        AgentInfo {
            id: AgentId(id.into()),
            name: name.into(),
            model: "test:model".into(),
            cwd: "/tmp".into(),
            skills: vec!["teach".into()],
        }
    }

    fn router_for_tests() -> (Router, String) {
        let token = auth::issue(KEY, "browser").expect("issue");
        (router(Office::new(KEY.to_vec())), token)
    }

    #[tokio::test]
    async fn the_roster_needs_a_token() {
        let (app, _) = router_for_tests();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/agents")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_roster_is_served_with_a_good_token() {
        let (app, token) = router_for_tests();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/agents")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let agents: Vec<AgentInfo> = serde_json::from_slice(&body).expect("roster json");
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn the_tail_is_bounded() {
        let (outbox, _inbox) = mpsc::channel(8);
        let room = Mutex::new(Room::new());
        let id = AgentId("a".into());
        room.lock().await.add(agent("a", "bob"), outbox);

        for n in 1..=300 {
            push_tail(
                &room,
                &id,
                TurnId(n),
                TurnEvent::Delta {
                    text: n.to_string(),
                },
            )
            .await;
        }

        let room = room.lock().await;
        let tail = &room.agents[&id.0].tail;
        assert_eq!(tail.len(), 200);
        assert_eq!(tail.front().expect("first kept").0, TurnId(101));
        assert_eq!(tail.back().expect("last kept").0, TurnId(300));
    }

    #[tokio::test]
    async fn a_clear_from_the_room_is_refused_and_never_reaches_the_agent() {
        let (outbox, mut inbox) = mpsc::channel(8);
        let room = Mutex::new(Room::new());
        let id = AgentId("a".into());
        room.lock().await.add(agent("a", "bob"), outbox);

        for text in ["/clear", "/quit"] {
            let reply = handle_browser_message(
                &room,
                FromBrowser::Send {
                    id: id.clone(),
                    text: text.into(),
                },
            )
            .await;
            assert!(
                matches!(reply, Some(ToBrowser::Refused { .. })),
                "{text} must be refused, got {reply:?}"
            );
            assert!(
                inbox.try_recv().is_err(),
                "{text} must never reach the agent"
            );
        }
    }

    #[tokio::test]
    async fn a_websocket_route_refuses_a_request_without_a_token() {
        let (app, _) = router_for_tests();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/ws/browser")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn a_second_agent_with_the_same_name_gets_a_suffix() {
        assert_eq!(unique_name(std::iter::empty(), "bob"), "bob");
        assert_eq!(unique_name(["bob"].into_iter(), "bob"), "bob-2");
        assert_eq!(unique_name(["bob", "bob-2"].into_iter(), "bob"), "bob-3");
    }

    #[tokio::test]
    async fn a_reconnected_agent_survives_the_forget_timer() {
        let (outbox, _inbox) = mpsc::channel(8);
        let room = Arc::new(Mutex::new(Room::new()));
        let id = AgentId("a".into());
        {
            let mut room = room.lock().await;
            room.add(agent("a", "bob"), outbox.clone());
            assert!(room.gone(&id.0), "the first close is reported");
            assert!(!room.gone(&id.0), "a close is reported only once");
        }
        forget_after(room.clone(), id.clone(), Duration::from_millis(1)).await;
        assert!(
            room.lock().await.agents.is_empty(),
            "a gone agent is forgotten"
        );

        {
            let mut room = room.lock().await;
            room.add(agent("a", "bob"), outbox);
            assert!(room.gone(&id.0));
            room.add(agent("a", "bob"), mpsc::channel(8).0);
        }
        forget_after(room.clone(), id.clone(), Duration::from_millis(1)).await;
        assert!(
            !room.lock().await.agents.is_empty(),
            "a reconnect cancels the removal"
        );
    }
}
