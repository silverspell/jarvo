# The Office Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking. Work in order; each task ends with
> green tests and a commit, and no task starts before the previous one's tests
> pass.

**Goal:** Several running Jarvos appear as characters in one web page; clicking
one opens its live transcript and a box that sends it commands.

**Architecture:** A separate office server process holds the room. Every Jarvo
connects to it as an equal WebSocket client and the browser connects the same
way. The first Jarvo that has the office enabled starts the server and then
joins it like everyone else. Inside a Jarvo nothing about the agent changes:
the office is a second consumer of the events the TUI already receives, and a
second producer of the commands the TUI already sends.

**Tech Stack:** Rust 2024, Tokio, `axum` (HTTP + WebSocket server),
`tokio-tungstenite` (client), `jsonwebtoken` (HS256), `serde_json`, plain
HTML/CSS/JS with no framework, `tmux` for spawned agents.

**Spec:** `docs/superpowers/specs/2026-10-25-jarvo-office-design.md` — read it
first. It explains *why*; this plan says *what*.

## Global Constraints

- **Never print to stdout or stderr from inside the Jarvo process.** The TUI
  owns the terminal. Use `tracing` only. The office *server*, being its own
  process, may print — that is the one exception.
- **Never log a token, a JWT, or a signing key.** Log the variable name, never
  its value.
- **Every new `PathBuf` option uses `value_parser = any_path`** and decides
  emptiness afterwards with `non_empty_path`, exactly like the existing options
  in `src/config.rs`. Clap's built-in `PathBuf` parser rejects an empty value
  and a line like `RIGAGENT_OFFICE_DIR=` in `.env` supplies exactly that, which
  would stop the program from starting.
- **Verify every Rig API against the source, not memory.** Rig 0.45 post-dates
  most training data: `ls ~/.cargo/registry/src/*/rig-core-0.45.0/src`. This
  plan touches Rig only where it quotes the existing code verbatim; do not
  invent Rig calls.
- **The office is optional in the same way MCP is.** A missing directory, an
  unreachable server, a failed spawn: a note in the conversation, never a fatal
  error, never a panic.
- **Before declaring any task done:** `cargo fmt`, then
  `cargo clippy --all-targets -- -D warnings`, then `cargo test`. All three must
  pass. Clippy warnings are failures.
- **Do not change the footer layout.** A test asserts it still fits 80 columns
  (`the_footer_shows_the_gauge_without_overflowing_eighty_columns`). Queue
  information goes in the transcript, not the footer.
- **Pin dependency versions by adding them with `cargo add`** and record the
  resolved version in `Cargo.toml`. Do not guess version numbers.

## Review Focus

Five things the spec implies that are easy to leave broken. Each has a test in
the task that owns the code.

1. **A blank `RIGAGENT_OFFICE_DIR=` in `.env` means "office off", not a startup
   failure.** Task 2.
2. **An office that is unreachable, dead, or never started must leave Jarvo
   working exactly as it does today** — a note, then business as usual. Task 8.
3. **Two Jarvos starting in the same moment must produce exactly one server**,
   with the loser joining it rather than failing. Task 5.
4. **A stalled office must never stall the agent or the TUI.** The office's
   event channel is lossy on purpose; a full one drops events and keeps going.
   Task 3.
5. **A request with no token, an expired token, or a tampered signature is
   refused** — on the HTTP routes and on the WebSocket handshake alike. Tasks 6
   and 7.

---

### Task 1: The wire format

**Files:**
- Create: `src/office/mod.rs` (just `pub mod protocol;` for now)
- Create: `src/office/protocol.rs`
- Modify: `src/main.rs:6-12` (add `mod office;`)
- Test: in `src/office/protocol.rs`, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: the types every later task sends and receives.

Define, with `#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]` and
`#[serde(tag = "type", rename_all = "snake_case")]` on the two enums:

```rust
pub struct AgentId(pub String);        // stable for the life of one process
pub struct TurnId(pub u64);            // monotonic per agent, starts at 1

pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,                  // display name
    pub model: String,                 // "deepseek:deepseek-chat"
    pub cwd: String,
    pub skills: Vec<String>,           // so the browser can offer /<skill>
}

pub enum FromAgent {                   // Jarvo -> server
    Hello { info: AgentInfo },
    Turn { turn: TurnId, event: TurnEvent },
}

pub enum ToAgent {                     // server -> Jarvo
    Prompt { text: String },
    Structured { text: String },
    Skill { name: String, request: String },
}

pub enum TurnEvent {                   // mirrors AgentEvent, plus queue news
    Started { prompt: String },
    Delta { text: String },
    ToolCall { name: String },
    ToolResult { name: String },
    Notice { text: String },
    ContextTokens { tokens: u64 },
    Completed { text: String },
    Failed { message: String },
    Queued { text: String, depth: usize },
}

pub enum ToBrowser {                   // server -> browser
    Roster { agents: Vec<AgentInfo> },
    Joined { info: AgentInfo },
    Left { id: AgentId },
    Turn { id: AgentId, turn: TurnId, event: TurnEvent },
    Tail { id: AgentId, events: Vec<(TurnId, TurnEvent)> },
}

pub enum FromBrowser {                 // browser -> server
    Select { id: AgentId },
    Send { id: AgentId, text: String },
    Spawn { cwd: String, name: Option<String> },
}
```

`TurnId` exists so a later feature can persist and replay a conversation; keep
it on every turn event even though nothing stores it yet.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_turn_event_round_trips_through_json() {
    let event = FromAgent::Turn {
        turn: TurnId(1),
        event: TurnEvent::Delta { text: "hi".into() },
    };
    let json = serde_json::to_string(&event).expect("serialize");
    assert!(json.contains(r#""type":"turn""#), "tagged: {json}");
    let back: FromAgent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, event);
}

#[test]
fn an_unknown_message_type_is_an_error_not_a_panic() {
    let result: Result<ToAgent, _> = serde_json::from_str(r#"{"type":"nope"}"#);
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test office::protocol`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Write the types above in `src/office/protocol.rs`**

Derive only; no methods, no logic.

- [ ] **Step 4: Run the tests**

Run: `cargo test office::protocol` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/office src/main.rs
git commit -m "Give the office a wire format"
```

---

### Task 2: The `--office-dir` option

**Files:**
- Modify: `src/config.rs` — add the CLI field, the `Config` field, and the line
  in `Config::resolve`
- Test: `src/config.rs`'s existing `mod tests`

**Interfaces:**
- Produces: `Config::office_dir: Option<PathBuf>`; `None` means the office is
  off for this Jarvo.

Follow the existing `shell_dir` option exactly: same `value_parser = any_path`,
same `non_empty_path` treatment in `resolve`, environment variable
`RIGAGENT_OFFICE_DIR`. Read `skills_dir` and `shell_dir` in that file and copy
their shape.

The default when the variable is absent is `None` — **off**. The spec is
explicit: a bare `rigagent` must not start listening on a port.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_blank_office_dir_means_off_rather_than_a_startup_failure() {
    let cli = Cli::try_parse_from(["rigagent", "--office-dir", ""])
        .expect("an empty path must parse, as it does for --shell-dir");
    let config = Config::resolve(cli).expect("resolve");
    assert!(config.office_dir.is_none());
}
```

If `Config::resolve` in the test environment needs a provider key, follow
whatever the neighbouring tests in that file already do.

- [ ] **Step 2: Run it and watch it fail** — `cargo test config::`

- [ ] **Step 3: Add the option**

`--office-dir`, env `RIGAGENT_OFFICE_DIR`, help text: "Directory holding the
office registration; unset means no office".

- [ ] **Step 4: Run the tests** — `cargo test config::` — Expected: PASS.

- [ ] **Step 5: Document it** in the `README.md` options and environment
  tables, beside `--shell-dir`.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs README.md
git commit -m "Let a Jarvo be told where the office is"
```

---

### Task 3: Two consumers for one event stream

**Files:**
- Modify: `src/agent.rs` — the `events: &mpsc::Sender<AgentEvent>` parameter
  threaded through `AgentTask::run`, `stream_turn`, `structured_turn`
- Test: `src/agent.rs`'s `mod tests`

**Interfaces:**
- Produces:

```rust
pub struct EventSink {
    primary: mpsc::Sender<AgentEvent>,          // the TUI: must not lose events
    extra: Option<mpsc::Sender<AgentEvent>>,    // the office: may lose them
}

impl EventSink {
    pub fn new(primary: mpsc::Sender<AgentEvent>) -> Self;
    pub fn with_extra(self, extra: mpsc::Sender<AgentEvent>) -> Self;
    pub async fn send(&self, event: AgentEvent) -> Result<(), SendError>;
}
```

`send` awaits on `primary` — backpressure there is correct, the transcript must
not lose a delta — and uses `try_send` on `extra`, discarding the event when the
office is full or gone. This asymmetry is the whole point of the type: **the
office must never be able to stall the agent or make the TUI stutter.** Count
the drops with `tracing::debug!` (no payload in the log).

Replace every `events.send(...)` in `agent.rs` with the sink. Keep
`AgentEvent`'s channel type as it is; do not reach for `tokio::sync::broadcast`
— a lagging subscriber there drops events for *every* consumer, which would
corrupt the transcript the TUI is building.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn the_primary_receives_every_event() {
    let (tx, mut rx) = mpsc::channel(4);
    let sink = EventSink::new(tx);
    sink.send(AgentEvent::Started).await.expect("send");
    assert!(matches!(rx.recv().await, Some(AgentEvent::Started)));
}

#[tokio::test]
async fn a_full_extra_channel_never_blocks_the_primary() {
    let (tx, mut rx) = mpsc::channel(8);
    let (extra_tx, extra_rx) = mpsc::channel(1);   // capacity 1, never drained
    let sink = EventSink::new(tx).with_extra(extra_tx);

    for _ in 0..5 {
        sink.send(AgentEvent::Delta("x".into()))
            .await
            .expect("a stalled office must not stall the agent");
    }
    drop(extra_rx);
    sink.send(AgentEvent::Started).await.expect("a dead office is not an error");
    assert_eq!(rx.recv().await.is_some(), true);
}
```

- [ ] **Step 2: Run them and watch them fail** — `cargo test agent::`

- [ ] **Step 3: Implement `EventSink` and thread it through `agent.rs`**

Mechanical: the three functions that take `&mpsc::Sender<AgentEvent>` take
`&EventSink` instead. `agent::start` builds the sink from the channel it already
creates at `src/agent.rs:181`.

- [ ] **Step 4: Run the whole suite** — `cargo test` — Expected: PASS, all 70+.

- [ ] **Step 5: Commit**

```bash
git add src/agent.rs
git commit -m "Let a second listener watch the agent without slowing it"
```

---

### Task 4: A queue you can see

**Files:**
- Modify: `src/agent.rs` — `AgentCommand`, `AgentEvent`, `AgentTask::run`
- Modify: `src/tui/mod.rs:241-244` (the busy rejection) and `submit`
- Modify: `src/tui/app.rs` — `apply_event`
- Test: all three files' `mod tests`

**Interfaces:**
- Consumes: `EventSink` from Task 3.
- Produces: two new `AgentEvent` variants:

```rust
Queued { text: String, depth: usize },   // something is waiting; depth includes it
Accepted { text: String },               // a prompt that did not come from this TUI
```

and one new field on the prompt-carrying `AgentCommand` variants:
`origin: Origin`, where `pub enum Origin { Tui, Office }`.

Behaviour:

- `AgentTask::run` keeps a `VecDeque<AgentCommand>`. While a turn is running it
  must still accept commands, so the turn loop becomes a `tokio::select!` over
  the stream and `commands.recv()`. Arriving commands are pushed onto the queue
  and announced with `Queued`. When the turn ends, the next command is popped
  and run.
- A command with `Origin::Office` emits `Accepted { text }` as its turn starts,
  so the terminal shows the question above the answer. A command with
  `Origin::Tui` does not: `submit` already pushed that line locally.
- `submit` no longer refuses when `app.busy`. Delete the rejection at
  `src/tui/mod.rs:241-244`; the queue is the answer now. Quit commands keep
  their place *before* that code — they must still work mid-turn.
- `App::apply_event` pushes `Role::System` `"queued: <text>"` for `Queued`, and
  `Role::User` `text` for `Accepted`.

The `select!` arm is the one piece of real concurrency here; everything else is
bookkeeping. Write it so that a command arriving mid-turn is queued and the
stream keeps being polled — not so that the stream is abandoned.

- [ ] **Step 1: Write the failing tests**

```rust
// src/tui/mod.rs
#[tokio::test]
async fn a_second_message_is_queued_rather_than_refused() {
    let (mut app, handle, mut rx) = fixture();
    app.busy = true;
    type_in(&mut app, "second thing");
    submit(&mut app, &handle).await.expect("submit");
    assert!(rx.try_recv().is_ok(), "a busy agent still takes the message");
    assert!(!matches!(app.status, Status::Error(_)));
}

// src/tui/app.rs
#[test]
fn a_queued_message_is_announced_and_an_office_prompt_is_echoed() {
    let mut app = app();
    app.apply_event(AgentEvent::Queued { text: "later".into(), depth: 1 });
    assert!(app.entries.last().expect("entry").text.contains("later"));

    app.apply_event(AgentEvent::Accepted { text: "from the room".into() });
    let last = app.entries.last().expect("entry");
    assert_eq!(last.role, Role::User);
    assert_eq!(last.text, "from the room");
}
```

- [ ] **Step 2: Run them and watch them fail** — `cargo test`

- [ ] **Step 3: Implement the queue in `AgentTask::run`, then the two UI cases**

- [ ] **Step 4: Run the whole suite** — `cargo test` — Expected: PASS.

Existing tests that assert the old refusal must be updated, not deleted: they
become assertions that the message is queued.

- [ ] **Step 5: Commit**

```bash
git add src/agent.rs src/tui/mod.rs src/tui/app.rs
git commit -m "Queue a message instead of refusing it"
```

---

### Task 5: Finding the office, and the race to start it

**Files:**
- Create: `src/office/registry.rs`
- Modify: `src/office/mod.rs`
- Test: in `src/office/registry.rs`

**Interfaces:**
- Produces:

```rust
pub struct Registration { pub port: u16, pub pid: u32, pub token: String }

/// Read `<dir>/office.json`, if it describes a live server.
pub fn find(dir: &Path) -> Option<Registration>;

/// Try to become the one that starts the office.
/// `Ok(true)` means this caller won and must now start the server;
/// `Ok(false)` means someone else won and the caller should `find` again.
pub fn claim(dir: &Path) -> std::io::Result<bool>;

pub fn publish(dir: &Path, registration: &Registration) -> std::io::Result<()>;
```

`claim` is won with `OpenOptions::new().create_new(true).open(dir.join("office.lock"))`
— atomic on every platform that matters, and the only correct way to settle the
race between two Jarvos starting in the same second. If the lock exists but
`office.json` names a pid that is no longer alive, remove both and retry once;
a crashed Jarvo must not lock the office out forever. Check liveness with
`kill(pid, 0)` semantics (`libc::kill` or an equivalent already in the tree) —
if that cannot be done portably, treat a registration older than 60 seconds
with no reachable port as stale.

Never log the `token` field.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn only_one_caller_wins_the_claim() {
    let dir = tempdir();                       // a fresh directory per test
    assert!(claim(&dir).expect("first"), "the first caller wins");
    assert!(!claim(&dir).expect("second"), "the second must join instead");
}

#[test]
fn a_registration_round_trips() {
    let dir = tempdir();
    let registration = Registration { port: 7777, pid: std::process::id(), token: "t".into() };
    publish(&dir, &registration).expect("publish");
    assert_eq!(find(&dir).expect("found").port, 7777);
}

#[test]
fn a_missing_directory_is_not_an_error() {
    assert!(find(Path::new("/nonexistent/rigagent-office")).is_none());
}
```

- [ ] **Step 2: Run them and watch them fail** — `cargo test office::registry`

- [ ] **Step 3: Implement the three functions**

- [ ] **Step 4: Run the tests** — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/office
git commit -m "Settle which Jarvo starts the office"
```

---

### Task 6: Tokens

**Files:**
- Create: `src/office/auth.rs`
- Modify: `Cargo.toml` (`cargo add jsonwebtoken`)
- Test: in `src/office/auth.rs`

**Interfaces:**
- Produces:

```rust
/// Load the HS256 signing key from `<dir>/office.key`, creating a random one
/// (32 bytes, file mode 0o600) the first time.
pub fn load_or_create_key(dir: &Path) -> std::io::Result<Vec<u8>>;

pub fn issue(key: &[u8], subject: &str) -> Result<String, Error>;   // 30-day expiry
pub fn verify(key: &[u8], token: &str) -> Result<Claims, Error>;

pub struct Claims { pub sub: String, pub exp: usize }
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_token_this_key_issued_verifies() {
    let key = b"0123456789abcdef0123456789abcdef";
    let token = issue(key, "browser").expect("issue");
    assert_eq!(verify(key, &token).expect("verify").sub, "browser");
}

#[test]
fn a_token_from_another_key_is_refused() {
    let token = issue(b"0123456789abcdef0123456789abcdef", "browser").expect("issue");
    assert!(verify(b"ffffffffffffffffffffffffffffffff", &token).is_err());
}

#[test]
fn a_tampered_token_is_refused() {
    let key = b"0123456789abcdef0123456789abcdef";
    let mut token = issue(key, "browser").expect("issue");
    token.pop();
    assert!(verify(key, &token).is_err());
}
```

- [ ] **Step 2: Run them and watch them fail** — `cargo test office::auth`

- [ ] **Step 3: Implement with `jsonwebtoken`'s `encode`/`decode`**

- [ ] **Step 4: Run the tests** — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/office
git commit -m "Sign and check the office's tokens"
```

---

### Task 7: The server

**Files:**
- Create: `src/office/server.rs`
- Modify: `Cargo.toml` (`cargo add axum tower-http --features ...` as needed)
- Modify: `src/main.rs` — a hidden `--serve-office <dir>` argument that runs the
  server instead of the TUI, so one binary does both
- Test: in `src/office/server.rs`

**Interfaces:**
- Consumes: `protocol`, `auth`, `registry`.
- Produces: `pub async fn serve(dir: &Path, bind: SocketAddr) -> Result<()>`.

Routes:

| Route | Purpose |
| --- | --- |
| `GET /` | the page (Task 9), token not required to load the shell |
| `GET /api/agents` | the roster, JSON, token required |
| `GET /ws/agent` | a Jarvo's socket, token required |
| `GET /ws/browser` | the browser's socket, token required |

State: a `tokio::sync::Mutex` over a struct holding each connected agent's
`AgentInfo`, a bounded tail (`VecDeque<(TurnId, TurnEvent)>`, cap 200 per agent)
and a `mpsc::Sender<ToAgent>` to reach it. Browser sockets get a
`broadcast::Sender<ToBrowser>` — here a lagging *browser* dropping frames is
acceptable, unlike the TUI.

The token arrives as `Authorization: Bearer <jwt>` on HTTP, and for the
WebSocket routes as a `?token=` query parameter (browsers cannot set headers on
a WebSocket handshake). **No cookies anywhere.** This is the spec's decision;
do not reach for a cookie because it is easier.

Default bind is `127.0.0.1:0` (a free port). Binding anywhere else is a flag,
and the README has to say what that exposes.

When an agent's socket closes, the server keeps its `AgentInfo` and tail for 60
seconds and tells the browsers `Left`, so a disappearing agent greys out before
it drops rather than vanishing mid-sentence. After that it is forgotten.

Note what the protocol cannot express, and keep it that way: there is no
`ToAgent` variant that clears a conversation or quits an agent. `/clear` and the
quit commands do not cross the network, by construction rather than by a check.

- [ ] **Step 1: Write the failing tests**

Use `tower::ServiceExt::oneshot` against the router, so no port is bound:

```rust
#[tokio::test]
async fn the_roster_needs_a_token() {
    let app = router_for_tests();
    let response = app.oneshot(Request::builder().uri("/api/agents").body(Body::empty()).unwrap())
        .await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_roster_is_served_with_a_good_token() { /* 200, empty array */ }

#[tokio::test]
async fn the_tail_is_bounded() {
    // push 300 events for one agent; assert the stored tail is 200
}
```

- [ ] **Step 2: Run them and watch them fail** — `cargo test office::server`

- [ ] **Step 3: Implement the router, the state and the two socket handlers**

- [ ] **Step 4: Run the tests** — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/office src/main.rs
git commit -m "Open the office"
```

---

### Task 8: A Jarvo joins the room

**Files:**
- Create: `src/office/client.rs`
- Modify: `src/agent.rs` — `start` wires the extra sink when the office is on
- Modify: `Cargo.toml` (`cargo add tokio-tungstenite`)
- Test: in `src/office/client.rs`

**Interfaces:**
- Consumes: `registry`, `protocol`, `EventSink` (Task 3), `AgentHandle`.
- Produces:

```rust
/// Join the office at `dir`, starting the server if nobody has.
/// Returns the channel the agent's events should also go to, plus a note for
/// the conversation. Any failure is a note, never an error.
pub async fn join(
    dir: &Path,
    info: AgentInfo,
    handle: AgentHandle,
) -> (Option<mpsc::Sender<AgentEvent>>, String);
```

The `AgentInfo` this Jarvo announces takes its `name` from `--office-name` if
given, otherwise the basename of the working directory; if the server already
holds that name, it appends `-2`, `-3` and so on. `id` is fresh per process:
a restarted Jarvo is a new character, by decision in the spec.

`join` does, in order: `registry::find`; if nothing, `registry::claim`; if won,
spawn `std::env::current_exe()` with `--serve-office <dir>` detached and wait
(polling `find`, up to 5 seconds) for the registration; connect the WebSocket;
send `Hello`; then run two loops — `AgentEvent` → `FromAgent::Turn` out, and
`ToAgent` → `AgentCommand` (with `Origin::Office`) in through `handle`.

**If anything fails, return `(None, note)`.** Jarvo must start and work exactly
as it does today when there is no office. This is Review Focus 2 and the spec's
sharpest anti-goal.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_unreachable_office_is_a_note_not_a_failure() {
    let dir = tempdir();                       // empty: no registration, no binary to spawn
    let (sink, note) = join(&dir, test_info(), test_handle()).await;
    assert!(sink.is_none());
    assert!(note.to_lowercase().contains("office"), "the note names the problem: {note}");
}

#[tokio::test]
async fn a_prompt_from_the_office_reaches_the_agent_with_its_origin() {
    // drive the inbound loop with a ToAgent::Prompt and assert an
    // AgentCommand::Prompt { origin: Origin::Office, .. } arrives on the handle
}
```

- [ ] **Step 2: Run them and watch them fail** — `cargo test office::client`

- [ ] **Step 3: Implement `join` and the two loops**

- [ ] **Step 4: Wire it into `agent::start`**, behind `config.office_dir`, adding
  the returned note to `Started::notes` like the MCP notes at `src/agent.rs:148`.

- [ ] **Step 5: Run the whole suite** — `cargo test` — Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/office src/agent.rs
git commit -m "Let a Jarvo take its seat in the room"
```

---

### Task 9: The room

**Files:**
- Create: `src/office/web/index.html`, `src/office/web/app.js`, `src/office/web/style.css`
- Modify: `src/office/server.rs` — serve them with `include_str!`
- Test: in `src/office/server.rs`

**Interfaces:**
- Consumes: `ToBrowser` / `FromBrowser` from Task 1.

Layout, from the spec: **the room on the left and larger, the inspector panel on
the right.** The panel must not squeeze the room into a strip — give the room at
least 60% of the width. One character per agent with its name and a status dot:
idle, thinking, running `<tool>`, waiting. Clicking a character selects it and
fills the panel with that agent's tail, then its live deltas, with a command box
under it. The box sends `FromBrowser::Send`. A `+ Agent` button asks for a
directory and sends `FromBrowser::Spawn`.

Plain HTML, CSS and JavaScript. No framework, no build step, no bundler. The
token comes from the URL's `?token=` on first load and is kept in
`localStorage`; **never in a cookie.** Deltas arrive continuously, so append to
a text node rather than re-rendering the panel.

The command box understands the same `/` commands the terminal does, minus the
destructive ones: `/json <prompt>` sends `FromBrowser::Send` whose text the
server turns into `ToAgent::Structured`, `/<skill> <request>` into
`ToAgent::Skill`, and anything else into `ToAgent::Prompt`. The skill names come
from the roster — add `skills: Vec<String>` to `AgentInfo` in Task 1's types and
fill it in Task 8 from `Started::skills`. `/clear`, `/exit`, `/quit` and `/bye`
are typed into the box as ordinary text and must not be special-cased into
anything: the protocol has no variant for them.

Pixel art is explicitly *not* part of this task: a coloured box with a name and
a dot is the deliverable. The visuals are a later plan.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_page_is_served() {
    let response = router_for_tests()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("body");
    assert!(String::from_utf8_lossy(&body).contains("<html"));
}
```

- [ ] **Step 2: Run it and watch it fail** — `cargo test office::server`

- [ ] **Step 3: Write the three files and serve them**

- [ ] **Step 4: Run the tests** — Expected: PASS.

- [ ] **Step 5: Look at it.** Start two Jarvos with `--office-dir`, open the
  printed URL, and confirm: both appear, selecting one shows its transcript,
  typing in the box reaches that agent, and a message sent while it is busy
  shows as queued in both the browser and the terminal.

- [ ] **Step 6: Commit**

```bash
git add src/office
git commit -m "Draw the room"
```

---

### Task 10: Starting a Jarvo from the room

**Files:**
- Create: `src/office/spawn.rs`
- Modify: `src/office/server.rs` — handle `FromBrowser::Spawn`
- Test: in `src/office/spawn.rs`

**Interfaces:**
- Produces:

```rust
/// The tmux command line that starts a Jarvo in `cwd`, named `name`.
pub fn tmux_command(exe: &Path, dir: &Path, cwd: &Path, name: &str) -> Vec<String>;

pub fn spawn(exe: &Path, dir: &Path, cwd: &Path, name: &str) -> std::io::Result<()>;
```

`tmux new-session -d -s jarvo-<name> -c <cwd> <exe> --office-dir <dir>`.

**The spawned Jarvo gets no shell.** Do not pass `--shell-dir`. Someone who can
reach the office can already drive every agent in it; letting the room mint new
agents with unconfirmed shell access on top of that is a bigger door than this
feature needs. If it is ever wanted, it is an explicit flag on the server with
its own line in the README.

Test `tmux_command` (a pure function); do not run tmux in a test.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn the_spawn_command_names_the_session_and_never_enables_the_shell() {
    let argv = tmux_command(
        Path::new("/usr/local/bin/rigagent"),
        Path::new("/home/me/.jarvo/office"),
        Path::new("/home/me/code/api"),
        "api",
    );
    assert_eq!(argv[0], "tmux");
    assert!(argv.windows(2).any(|w| w[0] == "-s" && w[1] == "jarvo-api"));
    assert!(argv.iter().any(|a| a == "--office-dir"));
    assert!(!argv.iter().any(|a| a == "--shell-dir"), "no shell from the room");
}
```

- [ ] **Step 2: Run it and watch it fail** — `cargo test office::spawn`

- [ ] **Step 3: Implement both functions and the server's handler**

A failed spawn answers the browser with a message; it never panics the server.

- [ ] **Step 4: Run the tests** — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/office
git commit -m "Hire a new agent from the room"
```

---

### Task 11: Write it down

**Files:**
- Modify: `README.md` — an "Office (optional)" section
- Modify: `AGENTS.md` — the module table and the invariants

**Interfaces:** none; this is the task that makes the rest usable.

The README section states, plainly:

- What the office is and how to turn it on (`--office-dir` / `RIGAGENT_OFFICE_DIR`).
- That the first Jarvo with it enabled starts the server, and the others join.
- That the server binds `127.0.0.1` by default, and that **exposing the port
  sends the bearer token and the whole conversation in cleartext** — put it
  behind a reverse proxy that terminates TLS.
- That **anyone who can open the office can drive every agent in it**, and an
  agent started with `--shell-dir` runs commands without confirmation.
- That agents started from the room get no shell.
- That a spawned agent runs in a `tmux` session named `jarvo-<name>`, so you can
  `ssh` in and attach.
- That the office is optional: no directory, no office, and a broken office is a
  note rather than a failure.

`AGENTS.md` gains `src/office/*` in the module table, and one invariant:
**the office never slows the agent or the TUI** — its event channel is lossy on
purpose, and that is why `EventSink` has two different send paths.

- [ ] **Step 1: Write both sections**

- [ ] **Step 2: Run the full check** — `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`

- [ ] **Step 3: Commit**

```bash
git add README.md AGENTS.md
git commit -m "Describe the office and what it opens"
```
