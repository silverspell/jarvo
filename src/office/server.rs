//! The office server: the page, the roster, and the two sockets.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{
    HeaderMap, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
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
use super::spawn;
use crate::command::{self, Input};

/// Events kept per agent so a browser arriving late can catch up.
const TAIL_CAP: usize = 200;
/// How long a departed agent stays visible.
const FORGET_AFTER: Duration = Duration::from_secs(60);
/// Browser broadcasts may lag; a stale frame is cheaper than a stalled room.
const BROADCAST_CAPACITY: usize = 256;
/// Prompts waiting for one agent socket.
const OUTBOX_CAPACITY: usize = 64;

const PAGE: &str = include_str!("web/index.html");
const SCRIPT: &str = include_str!("web/app.js");
const STYLE: &str = include_str!("web/style.css");

/// Shared state: the signing key, the room, and the fan-out to browsers.
#[derive(Clone)]
struct Office {
    key: Arc<Vec<u8>>,
    /// The office directory a spawned Jarvo is told to join.
    dir: Arc<PathBuf>,
    room: Arc<Mutex<Room>>,
    browsers: broadcast::Sender<ToBrowser>,
}

impl Office {
    fn new(key: Vec<u8>, dir: impl Into<PathBuf>) -> Self {
        let (browsers, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            key: Arc::new(key),
            dir: Arc::new(dir.into()),
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

    async fn join(&self, mut info: AgentInfo, outbox: mpsc::Sender<ToAgent>) -> (AgentInfo, u64) {
        let mut room = self.room.lock().await;
        info.name = unique_name(room.names(&info.id.0), &info.name);
        let generation = room.add(info.clone(), outbox);
        // Broadcasting under the lock keeps `Joined` and `Left` in the same
        // order the room changed, so a close cannot overtake a reconnect.
        self.broadcast(ToBrowser::Joined { info: info.clone() });
        drop(room);
        (info, generation)
    }

    async fn publish_turn(&self, id: &AgentId, generation: u64, turn: TurnId, event: TurnEvent) {
        if let Some(seq) = push_tail(&self.room, id, generation, turn.clone(), event.clone()).await
        {
            self.broadcast(ToBrowser::Turn {
                id: id.clone(),
                turn,
                seq,
                event,
            });
        }
    }

    /// Mark the agent disconnected. `Some` carries the generation the forget
    /// timer must use; `None` means the close came from a replaced socket.
    async fn agent_gone(&self, id: &AgentId, generation: u64) -> Option<u64> {
        let mut room = self.room.lock().await;
        let fresh = room.gone(&id.0, generation)?;
        self.broadcast(ToBrowser::Left { id: id.clone() });
        Some(fresh)
    }
}

#[derive(Default)]
struct Room {
    agents: HashMap<String, Agent>,
    /// Bumped on every join and close, so a stale socket or timer can tell it
    /// has been superseded.
    next_generation: u64,
}

struct Agent {
    info: AgentInfo,
    /// `(turn, seq, event)`, the newest last.
    tail: VecDeque<(TurnId, u64, TurnEvent)>,
    /// Monotonic per agent, starts at 1 for the first stored event.
    next_seq: u64,
    outbox: mpsc::Sender<ToAgent>,
    connected: bool,
    generation: u64,
}

impl Room {
    fn new() -> Self {
        Self::default()
    }

    fn next_generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Insert or reconnect an agent, preserving the tail and the sequence
    /// counter either way. Restarting the counter beside an old tail would
    /// make a browser drop the new events as already seen.
    fn add(&mut self, info: AgentInfo, outbox: mpsc::Sender<ToAgent>) -> u64 {
        let previous = self.agents.remove(&info.id.0);
        let (tail, next_seq) = previous
            .map(|agent| (agent.tail, agent.next_seq))
            .unwrap_or_default();
        let generation = self.next_generation();
        self.agents.insert(
            info.id.0.clone(),
            Agent {
                info,
                tail,
                next_seq,
                outbox,
                connected: true,
                generation,
            },
        );
        generation
    }

    fn names<'a>(&'a self, except: &'a str) -> impl Iterator<Item = &'a str> {
        self.agents
            .values()
            .filter(move |agent| agent.info.id.0 != except)
            .map(|agent| agent.info.name.as_str())
    }

    /// Live agents only: a plain HTTP client has no `Left` frame to explain a
    /// departed one, so it must not be offered a ghost.
    fn roster(&self) -> Vec<AgentInfo> {
        self.agents
            .values()
            .filter(|agent| agent.connected)
            .map(|agent| agent.info.clone())
            .collect()
    }

    /// Every agent still in its grace window, live or not.
    fn all_roster(&self) -> Vec<AgentInfo> {
        self.agents
            .values()
            .map(|agent| agent.info.clone())
            .collect()
    }

    /// The frames a browser needs on arrival: the roster, then a `Left` for
    /// every agent that has already closed. Without the second frame a page
    /// opened during the grace period would show a departed agent as live and
    /// never start its removal timer.
    fn welcome(&self) -> Vec<ToBrowser> {
        let mut messages = vec![ToBrowser::Roster {
            agents: self.all_roster(),
        }];
        messages.extend(
            self.agents
                .values()
                .filter(|agent| !agent.connected)
                .map(|agent| ToBrowser::Left {
                    id: agent.info.id.clone(),
                }),
        );
        messages
    }

    /// Mark the connection behind `generation` disconnected, returning the
    /// fresh generation for its forget timer. A replaced socket gets `None`,
    /// so it cannot mark a live reconnection gone or schedule its removal.
    fn gone(&mut self, id: &str, generation: u64) -> Option<u64> {
        let current = self
            .agents
            .get(id)
            .is_some_and(|agent| agent.connected && agent.generation == generation);
        if !current {
            return None;
        }
        let fresh = self.next_generation();
        let agent = self.agents.get_mut(id)?;
        agent.connected = false;
        agent.generation = fresh;
        Some(fresh)
    }

    /// Forget a departed agent only when its close is still the latest word.
    fn forget(&mut self, id: &str, generation: u64) {
        if self
            .agents
            .get(id)
            .is_some_and(|agent| !agent.connected && agent.generation == generation)
        {
            self.agents.remove(id);
        }
    }
}

/// Append a served event if it belongs to the current connection, returning
/// the sequence assigned to it. `None` means a replaced socket sent it.
async fn push_tail(
    room: &Mutex<Room>,
    id: &AgentId,
    generation: u64,
    turn: TurnId,
    event: TurnEvent,
) -> Option<u64> {
    let mut room = room.lock().await;
    let agent = room.agents.get_mut(&id.0)?;
    if agent.generation != generation {
        return None;
    }
    agent.next_seq += 1;
    let seq = agent.next_seq;
    agent.tail.push_back((turn, seq, event));
    while agent.tail.len() > TAIL_CAP {
        agent.tail.pop_front();
    }
    Some(seq)
}

/// Forget a departed agent unless its id has reconnected in the meantime.
async fn forget_after(room: Arc<Mutex<Room>>, id: AgentId, generation: u64, delay: Duration) {
    tokio::time::sleep(delay).await;
    room.lock().await.forget(&id.0, generation);
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
async fn handle_browser_message(
    room: &Mutex<Room>,
    dir: &Path,
    message: FromBrowser,
) -> Option<ToBrowser> {
    match message {
        FromBrowser::Select { id } => {
            let room = room.lock().await;
            let Some(events) = room
                .agents
                .get(&id.0)
                .map(|agent| agent.tail.iter().cloned().collect())
            else {
                return Some(refused(id, "no such agent"));
            };
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
        FromBrowser::Spawn { cwd, name } => {
            let cwd = PathBuf::from(cwd);
            let name = match name {
                Some(name) if !name.trim().is_empty() => name,
                _ => cwd
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "agent".into()),
            };
            let started = std::env::current_exe()
                .map_err(|err| format!("cannot find this program: {err}"))
                .and_then(|exe| {
                    spawn::spawn(&exe, dir, &cwd, &name)
                        .map_err(|err| format!("cannot start the agent: {err}"))
                });
            match started {
                // The new Jarvo announces itself with `Joined` on connect.
                Ok(()) => None,
                Err(reason) => Some(refused(AgentId(String::new()), &reason)),
            }
        }
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
        .route("/app.js", get(script_handler))
        .route("/style.css", get(style_handler))
        .merge(http)
        .merge(sockets)
        .with_state(state)
}

async fn page_handler() -> Html<&'static str> {
    Html(PAGE)
}

async fn script_handler() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/javascript; charset=utf-8")], SCRIPT)
}

async fn style_handler() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/css; charset=utf-8")], STYLE)
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
    let welcome = state.room.lock().await.welcome();
    for message in &welcome {
        if !send_json(&mut socket, message).await {
            return;
        }
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
                if let Some(reply) = handle_browser_message(&state.room, &state.dir, message).await
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
    let mut connection: Option<(AgentId, u64)> = None;

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
                    // A socket owns exactly one registration; a later Hello
                    // would orphan the first entry.
                    Ok(FromAgent::Hello { .. }) if connection.is_some() => continue,
                    Ok(FromAgent::Hello { info }) => {
                        let (info, generation) = state.join(info, outbox.clone()).await;
                        connection = Some((info.id, generation));
                    }
                    Ok(FromAgent::Turn { turn, event }) => {
                        if let Some((id, generation)) = &connection {
                            state.publish_turn(id, *generation, turn, event).await;
                        }
                    }
                    Err(_) => continue,
                }
            }
        }
    }

    if let Some((id, generation)) = connection
        && let Some(generation) = state.agent_gone(&id, generation).await
    {
        tokio::spawn(forget_after(
            state.room.clone(),
            id,
            generation,
            FORGET_AFTER,
        ));
    }
}

/// Run the office server until it stops.
///
/// This is the only place the process prints: it is its own process, and the
/// URL carries the token, so it is printed rather than logged.
pub async fn serve(dir: &Path, bind: &str) -> Result<()> {
    let dir = super::absolute_dir(dir)
        .with_context(|| format!("cannot resolve the office directory {}", dir.display()))?;
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("`{bind}` is not a socket address"))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the office to `{bind}`"))?;
    let bound = listener
        .local_addr()
        .context("cannot read the office address")?;
    let host = advertised_host(bound);

    let key = auth::load_or_create_key(&dir)
        .with_context(|| format!("cannot load the office key in {}", dir.display()))?;
    let token = auth::issue(&key, "office").context("cannot issue the office token")?;
    let state = Office::new(key, dir.clone());

    let registration = Registration {
        host: host.clone(),
        port: bound.port(),
        pid: std::process::id(),
        token: token.clone(),
    };
    registry::publish(&dir, &registration)
        .with_context(|| format!("cannot write the office registration in {}", dir.display()))?;

    println!(
        "Jarvo office: http://{host}:{}/?token={token}",
        bound.port()
    );
    axum::serve(listener, router(state))
        .await
        .context("the office server stopped")?;
    Ok(())
}

/// The host agents and the room should dial. A wildcard bind is not an address
/// a browser can use, so it advertises loopback; anything else advertises the
/// bound IP, bracketed when IPv6 so it forms a URL.
fn advertised_host(addr: SocketAddr) -> String {
    let ip = addr.ip();
    if ip.is_unspecified() {
        "127.0.0.1".into()
    } else if ip.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
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
        (router(Office::new(KEY.to_vec(), Path::new("/tmp"))), token)
    }

    #[tokio::test]
    async fn the_page_is_served() {
        let (app, _) = router_for_tests();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let page = String::from_utf8_lossy(&body);
        assert!(page.contains("<html"));
        assert!(
            page.contains("/app.js") && page.contains("/style.css"),
            "the page loads its script and stylesheet"
        );
    }

    #[tokio::test]
    async fn the_scripts_are_served() {
        let (app, _) = router_for_tests();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app.js")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .expect("a content type");
        assert!(
            content_type.contains("javascript"),
            "app.js is served as JavaScript: {content_type}"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let script = String::from_utf8_lossy(&body);
        assert!(
            script.contains("localStorage"),
            "the token is kept in localStorage"
        );
        assert!(
            script.contains("WebSocket"),
            "the page opens a WebSocket to the office"
        );
        assert!(
            !script.contains("document.cookie"),
            "a token must never be kept in a cookie"
        );
        assert!(
            script.contains("/ws/browser"),
            "the page connects to the browser socket"
        );
        assert!(
            !script.contains("residual"),
            "tail/live duplication is decided by sequence, not by content"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/style.css")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .expect("a content type");
        assert!(
            content_type.contains("text/css"),
            "style.css is served as CSS: {content_type}"
        );
    }

    #[tokio::test]
    async fn a_reconnect_keeps_the_sequence_beside_the_old_tail() {
        let office = Office::new(KEY.to_vec(), Path::new("/tmp"));
        let (first_outbox, _first_inbox) = mpsc::channel(8);
        let (info, generation) = office.join(agent("a", "bob"), first_outbox).await;
        office
            .publish_turn(
                &info.id,
                generation,
                TurnId(1),
                TurnEvent::Started { prompt: "q".into() },
            )
            .await;

        let (second_outbox, _second_inbox) = mpsc::channel(8);
        let (_, generation) = office.join(info.clone(), second_outbox).await;
        office
            .publish_turn(
                &info.id,
                generation,
                TurnId(2),
                TurnEvent::Started {
                    prompt: "q2".into(),
                },
            )
            .await;

        let room = office.room.lock().await;
        let seqs: Vec<u64> = room.agents[&info.id.0]
            .tail
            .iter()
            .map(|(_, seq, _)| *seq)
            .collect();
        assert!(
            seqs == [1, 2],
            "a reconnect continues the sequence: {seqs:?}"
        );
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
    async fn a_gone_agent_is_not_in_the_http_roster() {
        let office = Office::new(KEY.to_vec(), Path::new("/tmp"));
        let (outbox, _inbox) = mpsc::channel(8);
        let (info, generation) = office.join(agent("a", "bob"), outbox).await;
        assert_eq!(office.roster().await.len(), 1);

        office
            .agent_gone(&info.id, generation)
            .await
            .expect("the socket closed");

        assert!(
            office.roster().await.is_empty(),
            "HTTP has no Left frame, so it must not report a departed agent"
        );
    }

    #[tokio::test]
    async fn selecting_an_unknown_agent_is_refused() {
        let room = Mutex::new(Room::new());
        let reply = handle_browser_message(
            &room,
            Path::new("/tmp"),
            FromBrowser::Select {
                id: AgentId("nope".into()),
            },
        )
        .await;
        match reply {
            Some(ToBrowser::Refused { id, reason }) => {
                assert_eq!(id.0, "nope");
                assert!(reason.contains("no such agent"), "{reason}");
            }
            other => panic!("an unknown agent must be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_tail_is_bounded() {
        let (outbox, _inbox) = mpsc::channel(8);
        let room = Mutex::new(Room::new());
        let id = AgentId("a".into());
        let generation = room.lock().await.add(agent("a", "bob"), outbox);

        for n in 1..=300 {
            let seq = push_tail(
                &room,
                &id,
                generation,
                TurnId(n),
                TurnEvent::Delta {
                    text: n.to_string(),
                },
            )
            .await
            .expect("the current socket appends");
            assert_eq!(seq, n);
        }

        let room = room.lock().await;
        let tail = &room.agents[&id.0].tail;
        assert_eq!(tail.len(), 200);
        assert_eq!(tail.front().expect("first kept").0, TurnId(101));
        assert_eq!(tail.front().expect("first kept").1, 101);
        assert_eq!(tail.back().expect("last kept").0, TurnId(300));
        assert_eq!(tail.back().expect("last kept").1, 300);
    }

    #[tokio::test]
    async fn turns_carry_increasing_sequences_and_the_tail_keeps_them() {
        let office = Office::new(KEY.to_vec(), Path::new("/tmp"));
        let (outbox, _inbox) = mpsc::channel(8);
        let (info, generation) = office.join(agent("a", "bob"), outbox).await;
        let mut turns = office.browsers.subscribe();

        office
            .publish_turn(
                &info.id,
                generation,
                TurnId(1),
                TurnEvent::Started { prompt: "q".into() },
            )
            .await;
        office
            .publish_turn(
                &info.id,
                generation,
                TurnId(1),
                TurnEvent::Completed { text: "a".into() },
            )
            .await;

        let mut broadcast_seqs = Vec::new();
        for _ in 0..2 {
            let Ok(ToBrowser::Turn { id, seq, .. }) = turns.try_recv() else {
                panic!("two turns are broadcast");
            };
            assert!(id == info.id, "the turn names the agent");
            broadcast_seqs.push(seq);
        }
        assert!(
            broadcast_seqs == [1, 2],
            "sequences increase per agent: {broadcast_seqs:?}"
        );

        let reply = handle_browser_message(
            &office.room,
            Path::new("/tmp"),
            FromBrowser::Select {
                id: info.id.clone(),
            },
        )
        .await;
        let Some(ToBrowser::Tail { events, .. }) = reply else {
            panic!("a select returns the tail");
        };
        let tail_seqs: Vec<u64> = events.iter().map(|(_, seq, _)| *seq).collect();
        assert!(
            tail_seqs == [1, 2],
            "the tail preserves the sequences: {tail_seqs:?}"
        );
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
                Path::new("/tmp"),
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
        for route in ["/ws/browser", "/ws/agent"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(route)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{route} needs a token"
            );
        }
    }

    #[tokio::test]
    async fn a_late_browser_is_told_which_agents_have_left() {
        let office = Office::new(KEY.to_vec(), Path::new("/tmp"));
        let (outbox, _inbox) = mpsc::channel(8);
        let (info, generation) = office.join(agent("a", "bob"), outbox).await;
        office
            .agent_gone(&info.id, generation)
            .await
            .expect("the socket closed");

        let welcome = office.room.lock().await.welcome();
        assert_eq!(
            welcome,
            vec![
                ToBrowser::Roster {
                    agents: vec![info.clone()]
                },
                ToBrowser::Left { id: info.id },
            ],
            "a page opened during the grace period sees the roster and the departure"
        );
    }

    #[tokio::test]
    async fn a_live_agent_gets_no_bogus_left_on_welcome() {
        let office = Office::new(KEY.to_vec(), Path::new("/tmp"));
        let (outbox, _inbox) = mpsc::channel(8);
        let (info, _) = office.join(agent("a", "bob"), outbox).await;

        let welcome = office.room.lock().await.welcome();
        assert_eq!(
            welcome,
            vec![ToBrowser::Roster { agents: vec![info] }],
            "only departed agents get a Left"
        );
    }

    #[tokio::test]
    async fn a_reconnect_before_the_old_socket_closes_stays_live() {
        let office = Office::new(KEY.to_vec(), Path::new("/tmp"));
        let mut frames = office.browsers.subscribe();

        let (first_outbox, _first_inbox) = mpsc::channel(8);
        let (info, stale) = office.join(agent("a", "bob"), first_outbox).await;
        let (second_outbox, _second_inbox) = mpsc::channel(8);
        let (_, current) = office.join(info.clone(), second_outbox).await;

        for _ in 0..2 {
            assert!(
                matches!(frames.try_recv(), Ok(ToBrowser::Joined { .. })),
                "each join is announced in order"
            );
        }

        assert!(
            office.agent_gone(&info.id, stale).await.is_none(),
            "the replaced socket cannot close the live one"
        );
        assert!(
            matches!(
                frames.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "a stale close must not send Left after the reconnect's Joined"
        );
        let room = office.room.lock().await;
        let agent = &room.agents[&info.id.0];
        assert!(agent.connected, "the reconnected agent is still live");
        assert_eq!(agent.generation, current);
    }

    #[test]
    fn a_wildcard_bind_advertises_loopback_and_other_binds_advertise_themselves() {
        assert_eq!(
            advertised_host("0.0.0.0:8080".parse().expect("addr")),
            "127.0.0.1"
        );
        assert_eq!(
            advertised_host("[::]:8080".parse().expect("addr")),
            "127.0.0.1"
        );
        assert_eq!(
            advertised_host("192.168.1.10:8080".parse().expect("addr")),
            "192.168.1.10"
        );
        assert_eq!(
            advertised_host("[::1]:8080".parse().expect("addr")),
            "[::1]",
            "an IPv6 host stays usable in a URL"
        );
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
        let closed = {
            let mut room = room.lock().await;
            let generation = room.add(agent("a", "bob"), outbox.clone());
            let closed = room.gone(&id.0, generation).expect("the first close");
            assert!(
                room.gone(&id.0, generation).is_none(),
                "a close is reported only once"
            );
            closed
        };
        forget_after(room.clone(), id.clone(), closed, Duration::from_millis(1)).await;
        assert!(
            room.lock().await.agents.is_empty(),
            "a gone agent is forgotten"
        );

        let closed = {
            let mut room = room.lock().await;
            let generation = room.add(agent("a", "bob"), outbox);
            room.gone(&id.0, generation).expect("a close")
        };
        room.lock().await.add(agent("a", "bob"), mpsc::channel(8).0);
        forget_after(room.clone(), id.clone(), closed, Duration::from_millis(1)).await;
        assert!(
            !room.lock().await.agents.is_empty(),
            "a reconnect cancels the removal"
        );
    }

    #[tokio::test]
    async fn a_flapped_agent_gets_a_fresh_forget_window() {
        let room = Mutex::new(Room::new());
        let id = AgentId("a".into());

        let first = room.lock().await.add(agent("a", "bob"), mpsc::channel(8).0);
        let first_timer = room.lock().await.gone(&id.0, first).expect("first close");
        let second = room.lock().await.add(agent("a", "bob"), mpsc::channel(8).0);
        let second_timer = room.lock().await.gone(&id.0, second).expect("second close");

        room.lock().await.forget(&id.0, first_timer);
        assert!(
            room.lock().await.agents.contains_key(&id.0),
            "the first timer must not shorten the latest close's window"
        );

        room.lock().await.forget(&id.0, second_timer);
        assert!(
            !room.lock().await.agents.contains_key(&id.0),
            "the latest close's timer forgets the agent"
        );
    }

    #[tokio::test]
    async fn a_stale_socket_cannot_mark_a_reconnected_agent_gone() {
        let office = Office::new(KEY.to_vec(), Path::new("/tmp"));
        let (first_outbox, _first_inbox) = mpsc::channel(8);
        let (info, stale) = office.join(agent("a", "bob"), first_outbox).await;

        let (second_outbox, _second_inbox) = mpsc::channel(8);
        let (_, current) = office.join(info.clone(), second_outbox).await;
        assert_ne!(stale, current, "a reconnect gets a fresh generation");

        let mut left = office.browsers.subscribe();
        assert!(
            office.agent_gone(&info.id, stale).await.is_none(),
            "a stale close is ignored"
        );
        assert!(
            matches!(left.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "a stale close must not announce a Left"
        );
        {
            let room = office.room.lock().await;
            let agent = &room.agents[&info.id.0];
            assert!(agent.connected, "the live connection is still connected");
            assert_eq!(agent.generation, current);
        }

        forget_after(
            office.room.clone(),
            info.id.clone(),
            stale,
            Duration::from_millis(1),
        )
        .await;
        assert!(
            office.room.lock().await.agents.contains_key(&info.id.0),
            "an old timer cannot remove the live agent"
        );
    }
}
