# The Office Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan task-by-task —
> a fresh implementer per task and a fresh reviewer before the next one starts.
> That method was chosen for this plan, not merely suggested. Steps use
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
`tokio-tungstenite` (client), `jsonwebtoken` (HS256) with `rand` for the
signing key, `serde_json`, plain HTML/CSS/JS with no framework, `tmux` for
spawned agents. Tests add `tempfile` and `tower`.

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
- **This repository is indexed by CodeGraph; use it before grep or reading
  files.** One `codegraph_explore` call with the names you need
  (`codegraph_explore "AgentTask AgentEvent EventSink"`, or
  `codegraph explore "<names>"` in a shell) returns those symbols' current
  line-numbered source plus who calls them — which is exactly what each task's
  "Modify:" list requires you to understand before you touch it. It is faster
  and more accurate than a grep-and-read loop, and it shows the blast radius a
  signature change will have on callers you did not know about.
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
- **The dependency set is exactly this, and nothing more:** `axum` (Task 7),
  `tokio-tungstenite` (Task 8), `jsonwebtoken` (Task 6), `rand` (Task 6 — the
  32 random bytes of the signing key; nothing in the tree generates randomness
  today), and as `[dev-dependencies]` `tempfile` (Tasks 5 and 8) and `tower`
  with its `util` feature (Tasks 7 and 9, for `ServiceExt::oneshot`).
  `tower-http` is **not** needed: the three static files are returned by
  handlers that hold `include_str!`, not by a file-serving layer. `serde`,
  `serde_json` and `tokio` are already there.
- **Assert with `assert!`, never `assert_eq!(x, true)`.** Clippy's
  `bool_assert_comparison` fires on the second form, and a clippy warning is a
  failed task here.

## Review Focus

Five things the spec implies that are easy to leave broken. Each has a test in
the task that owns the code.

1. **A blank `RIGAGENT_OFFICE_DIR=` in `.env` means "office off", not a startup
   failure.** Task 2.
2. **An office that is unreachable, dead, or never started must leave Jarvo
   working exactly as it does today** — a note, then business as usual. Task 8.
3. **Two Jarvos starting in the same moment must produce exactly one server**,
   with the loser joining it rather than failing — and a Jarvo that wins the
   claim and then dies before the server registers must not lock the office
   out forever. Task 5.
4. **A stalled office must never stall the agent or the TUI, and the server it
   spawns must never write to the terminal the TUI owns.** The office's event
   channel is lossy on purpose; a full one drops events and keeps going
   (Task 3), and the spawned server's three streams are `Stdio::null()`
   (Task 8).
5. **A request with no token, an expired token, or a tampered signature is
   refused** — on the HTTP routes and on the WebSocket handshake alike. Tasks 6
   and 7.

---

### Task 1: The wire format and the one command parser

**Files:**
- Create: `src/office/mod.rs` (just `pub mod protocol;` for now)
- Create: `src/office/protocol.rs`
- Create: `src/command.rs` — the `/` parser, lifted out of `tui/mod.rs::submit`
- Modify: `src/main.rs:6-12` (add `mod command;` and `mod office;`)
- Modify: `src/tui/mod.rs:225-281` — `submit` asks the parser instead of
  parsing inline, and `skill_command` moves into `src/command.rs`
- Test: `#[cfg(test)] mod tests` in `src/office/protocol.rs` and in
  `src/command.rs`

**Interfaces:**
- Produces: the types every later task sends and receives, and the single
  parser Tasks 7 and 9 share with the terminal.

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
    pub skills: Vec<String>,           // so the browser can list /<skill>
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
    Refused { id: AgentId, reason: String },
}

pub enum FromBrowser {                 // browser -> server
    Select { id: AgentId },
    Send { id: AgentId, text: String },   // raw, exactly as typed
    Spawn { cwd: String, name: Option<String> },
}
```

`TurnId` exists so a later feature can persist and replay a conversation; keep
it on every turn event even though nothing stores it yet.

Two rulings the protocol needs, so that Tasks 7, 8 and 9 agree:

- **`TurnId` is minted by the Jarvo-side client (Task 8), never by the
  server.** It starts at 1 and increments when the client sees
  `AgentEvent::Started`; every event up to the next `Started` carries that id.
  An event that belongs to no turn — a startup `Notice`, or a `Queued` while
  another turn runs — carries the id of the turn in progress, and `TurnId(0)`
  before the first one. It is a stable handle for the session sharing the spec
  defers, nothing more.
- **`Accepted` never crosses the wire.** That variant (Task 4) exists so the
  *terminal* shows the question above the answer when a prompt came from the
  room; the room drew that line itself when it sent it. The client drops it,
  which is why `TurnEvent` has `Queued` and no `Accepted`.

**The parser.** The spec's decision 9 — the room sends prompts, `/json` and
skills, and not the destructive commands — is only true by construction if
there is exactly one parser. Today `submit` parses inline
(`src/tui/mod.rs:253-279`) and copying that shape into the server would be two
parsers drifting apart. Lift it:

```rust
// src/command.rs
pub enum Input {
    Prompt(String),
    Structured(String),                        // /json <prompt>
    Skill { name: String, request: String },   // /<skill> <request>
    JsonWithoutAPrompt,                        // a bare /json
    Clear,                                     // /clear
    Quit,                                      // /exit, /quit, /bye
}

pub fn parse(text: &str, skills: &[String]) -> Input;
```

`parse` is pure: no `App`, no I/O, no transcript pushes. `submit` keeps the
lines it writes and the quit handling it has today and takes its decision from
`Input` instead of from `strip_prefix`; the quit check stays exactly where it
is, before the busy guard and before the input is consumed.

Task 7's server calls this same `parse`. `Input::Clear` and `Input::Quit`
arriving from the room are answered with `ToBrowser::Refused` and go no
further: nobody wipes a conversation or closes a Jarvo somebody else is
sitting in front of. There is deliberately no `ToAgent` variant for either, so
the refusal sits at the edge of a protocol that could not carry them anyway.

- [ ] **Step 1: Write the failing tests**

```rust
// src/office/protocol.rs
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

// src/command.rs
#[test]
fn the_parser_tells_the_shapes_apart() {
    let skills = vec!["teach".to_string()];
    assert!(matches!(parse("hello", &skills), Input::Prompt(_)));
    assert!(matches!(parse("/json do it", &skills), Input::Structured(p) if p == "do it"));
    assert!(matches!(parse("/json", &skills), Input::JsonWithoutAPrompt));
    assert!(matches!(parse("/teach rust", &skills), Input::Skill { name, .. } if name == "teach"));
    assert!(matches!(parse("/clear", &skills), Input::Clear));
    assert!(matches!(parse("/bye", &skills), Input::Quit));
    assert!(
        matches!(parse("/nope", &skills), Input::Prompt(_)),
        "an unknown slash command is ordinary text, as it is today"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test office::protocol command::`
Expected: FAIL — neither module exists.

- [ ] **Step 3: Write the types in `src/office/protocol.rs`**

Derive only; no methods, no logic.

- [ ] **Step 4: Move the parse out of `submit` into `src/command.rs`**

`submit` keeps its structure and its transcript lines; only the decision moves.
The existing `submit` tests must keep passing unchanged — that is the proof the
lift was behaviour-preserving.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test` — Expected: PASS, including every existing `submit` test.

- [ ] **Step 6: Commit**

```bash
git add src/office src/command.rs src/main.rs src/tui/mod.rs
git commit -m "Give the office a wire format and one command parser"
```

---

### Task 2: The office options

**Files:**
- Modify: `src/config.rs` — four new `Cli` fields, two new `Config` fields, and
  the lines in `Config::resolve`
- Test: `src/config.rs`'s existing `mod tests`

**Interfaces:**
- Produces, on `Config` (the resolved configuration the TUI runs on):

```rust
pub office_dir: Option<PathBuf>,   // None means the office is off
pub office_name: Option<String>,   // None means "name me after my directory"
```

- Produces, on `Cli` only (read before `Config::resolve`, because the server
  process needs neither a provider nor a key):

```rust
pub office_bind: String,              // default "127.0.0.1:0"
pub serve_office: Option<PathBuf>,    // hidden; set means "be the server"
```

`--office-dir` follows the existing `shell_dir` exactly: `value_parser =
any_path`, `non_empty_path` in `resolve`, environment variable
`RIGAGENT_OFFICE_DIR`. Read `skills_dir` and `shell_dir` in that file and copy
their shape. The default when the variable is absent is `None` — **off**; the
spec is explicit that a bare `rigagent` must not start listening on a port.

`--office-name` (env `RIGAGENT_OFFICE_NAME`) is a `String`, so the empty-path
trap does not apply; an empty value resolves to `None`. Task 8 uses it, Task 10
passes it to a Jarvo started from the room.

`--office-bind` (env `RIGAGENT_OFFICE_BIND`, default `"127.0.0.1:0"`) is the
spec's deliberate flag: port 0 picks a free port, and reaching the office from
another machine means setting this to something else on purpose. It stays a
`String` and is parsed into a `SocketAddr` by the server in Task 7, so a typo
is the server's error message rather than a refusal to start the TUI. A blank
value — `RIGAGENT_OFFICE_BIND=` in `.env` — means the default, not an empty
address.

`--serve-office <dir>` is `hide = true`: it is how one binary is also the
server, not a thing a person types. It is a `PathBuf`, so it takes
`value_parser = any_path` like every other path option here, even though
nothing sets it from `.env`. **`src/main.rs` branches on it after
parsing `Cli` and before `Config::resolve`**, because the server needs no
provider, no model and no API key, and `resolve` fails without them. Clap
stays in `config.rs`, as `AGENTS.md` requires; `main.rs` holds only the branch.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_blank_office_dir_means_off_rather_than_a_startup_failure() {
    let cli = Cli::try_parse_from(["rigagent", "--office-dir", ""])
        .expect("an empty path must parse, as it does for --shell-dir");
    assert_eq!(non_empty_path(cli.office_dir), None);
}

#[test]
fn the_office_binds_to_localhost_on_a_free_port_unless_told_otherwise() {
    let cli = Cli::try_parse_from(["rigagent"]).expect("parse");
    assert_eq!(cli.office_bind, "127.0.0.1:0");
}

#[test]
fn serving_the_office_is_a_separate_mode() {
    let cli = Cli::try_parse_from(["rigagent", "--serve-office", "/tmp/office"])
        .expect("parse");
    assert_eq!(cli.serve_office.as_deref(), Some(Path::new("/tmp/office")));
}
```

Follow whatever the neighbouring tests in that file already do about a provider
key; the first test deliberately checks `non_empty_path` directly, the way
`a_blank_shell_directory_means_off` does, so it needs no resolved `Config`.

- [ ] **Step 2: Run them and watch them fail** — `cargo test config::`

- [ ] **Step 3: Add the four options and the two `Config` fields**

Help text: `--office-dir` "Directory holding the office registration; unset
means no office", `--office-name` "This agent's name in the office; defaults to
the directory's name", `--office-bind` "Address the office server listens on",
`--serve-office` hidden.

- [ ] **Step 4: Run the tests** — `cargo test config::` — Expected: PASS.

- [ ] **Step 5: Document them** in the `README.md` options and environment
  tables, beside `--shell-dir`. `--serve-office` is internal and stays out.

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
    assert!(
        rx.recv().await.is_some(),
        "the TUI keeps receiving after the office is gone"
    );
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
- Modify: `src/tui/mod.rs` — the busy rejection in `submit` (it is at
  `241-244` today; Task 1 moves the parse out of that function, so find the
  `app.busy` guard rather than trusting the line number)
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
- `submit` no longer refuses when `app.busy`. Delete that guard; the queue is
  the answer now. Quit commands keep their place *before* it — they must still
  work mid-turn.
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
race between two Jarvos starting in the same second.

**The winner writes its own pid into the lock file** before it returns, because
the lock can outlive it: a Jarvo that wins and then dies before the server
registers leaves a lock with no `office.json` beside it, and a plan that only
expires a dead pid *in the registration* would lock the office out forever.
So `claim` treats a lock as stale when any of these holds, removes it together
with any `office.json`, and retries exactly once:

- the lock names a pid that is no longer alive;
- the lock names no readable pid and is older than 60 seconds;
- there is no `office.json` and the lock is older than 60 seconds (the winner
  died mid-startup).

Liveness is `kill(pid, 0)` semantics. There is no `libc` in this tree and this
plan does not add one, so use `kill -0` through `std::process::Command`, or
`/proc`-free platform-agnostic age as the fallback above; either way an
unreadable answer counts as "not alive" only once the 60 seconds have passed,
so a slow start is never mistaken for a crash.

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

#[test]
fn a_lock_whose_winner_died_before_registering_is_reclaimed() {
    let dir = tempdir();
    std::fs::write(dir.join("office.lock"), "999999").expect("a dead pid");
    assert!(
        claim(&dir).expect("reclaim"),
        "a lock with no registration and a dead owner must not hold the office"
    );
}
```

`tempdir` is `tempfile::tempdir()`, kept in `[dev-dependencies]`.

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
- Modify: `Cargo.toml` — `cargo add axum`, plus `tower` with its `util` feature
  as a dev-dependency. Not `tower-http`: the static files are `include_str!`
  in handlers.
- Modify: `src/main.rs` — the branch on `Cli::serve_office` from Task 2, taken
  **before `Config::resolve`**, so one binary is also the server and the server
  starts without a provider or an API key
- Test: in `src/office/server.rs`

**Interfaces:**
- Consumes: `protocol`, `auth`, `registry`.
- Produces: `pub async fn serve(dir: &Path, bind: &str) -> Result<()>`. `bind`
  is `Cli::office_bind` verbatim; `serve` parses it into a `SocketAddr` itself,
  so a typo is this process's error message rather than a TUI that refuses to
  start.

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

The default `127.0.0.1:0` comes from `--office-bind` (Task 2), so reaching the
office from another machine is a deliberate flag and the README has to say what
that exposes. Once the listener is up, `serve` reads the real port back from it
(port 0 means the kernel chose it), issues a browser token, writes the
registration with `registry::publish`, and prints the URL carrying that token.
This is the one place in this codebase allowed to print, because the server is
its own process — and the URL is printed, never logged, because it contains a
token.

When an agent's socket closes the server sends `ToBrowser::Left { id }` once,
keeps that agent's `AgentInfo` and tail for 60 seconds so a browser arriving
late still sees its last words, and then forgets it. **The grey-out and the
removal are the browser's own timer, not a second frame** (Task 9): a browser
that connects during those 60 seconds never saw the `Left` that started the
clock, so a "now really gone" message would reach nobody who needed it. A
`Joined` with the same id cancels the timer.

`FromBrowser::Send` carries raw text, exactly as typed. The server hands it to
`command::parse` from Task 1 — the shared parser, not a copy — and maps
`Input::Structured` to `ToAgent::Structured`, `Input::Skill` to
`ToAgent::Skill`, `Input::Prompt` to `ToAgent::Prompt`, and
`Input::JsonWithoutAPrompt`, `Input::Clear` and `Input::Quit` to
`ToBrowser::Refused`. Note what the protocol cannot express, and keep it that
way: there is no `ToAgent` variant that clears a conversation or quits an
agent. `/clear` and the quit commands do not cross the network by
construction, not by a check.

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

#[tokio::test]
async fn a_clear_from_the_room_is_refused_and_never_reaches_the_agent() {
    // feed FromBrowser::Send { text: "/clear" } to the handler for a connected
    // fake agent; assert a ToBrowser::Refused goes back and that the agent's
    // ToAgent channel is still empty. Same for "/quit".
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
    bind: &str,              // Cli::office_bind, handed on to a server we spawn
    info: AgentInfo,
    handle: AgentHandle,
) -> (Option<mpsc::Sender<AgentEvent>>, String);
```

`agent::start` builds the `AgentInfo`: `name` is `config.office_name` (Task 2)
when it is set and the basename of the working directory otherwise, and the
*server* appends `-2`, `-3` and so on when that name is taken — de-duplication
belongs to the one process that can see every name. `id` is fresh per process:
a restarted Jarvo is a new character, by decision in the spec. `skills` is the
list `Started::skills` already carries.

`join` does, in order: `registry::find`; if nothing, `registry::claim`; if won,
spawn `std::env::current_exe()` with
`--serve-office <dir> --office-bind <bind>` and wait (polling `find`, up to 5
seconds) for the registration; connect the WebSocket; send `Hello`; then run
two loops — `AgentEvent` → `FromAgent::Turn` out, and `ToAgent` →
`AgentCommand` (with `Origin::Office`) in through `handle`.

**The spawn sets `Stdio::null()` on stdin, stdout and stderr.** A child
inherits them by default, so the server's startup line — or its panic — would
print straight onto the running TUI, which is the spec's sharpest anti-goal and
exactly why `src/mcp.rs` pipes a server's stderr into `tracing` instead. The
URL the server prints therefore reaches nobody; `join` reads the port and token
back out of the registration and puts the address in its note, so it arrives in
the conversation the way the MCP notes do.

**If anything fails, return `(None, note)`.** Jarvo must start and work exactly
as it does today when there is no office. This is Review Focus 2 and the spec's
sharpest anti-goal.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_office_that_cannot_be_claimed_is_a_note_not_a_failure() {
    // A file where the directory should be: `find` finds nothing, `claim`
    // cannot create the lock inside it, and `join` returns before it would
    // spawn anything. Do NOT point this test at an empty temporary directory:
    // `claim` would win, and under `cargo test` `current_exe()` is the test
    // binary — the test would spawn a copy of itself, wait five seconds, and
    // leave the process behind.
    let dir = tempdir();
    let path = dir.join("not-a-directory");
    std::fs::write(&path, b"").expect("write");

    let (sink, note) = join(&path, "127.0.0.1:0", test_info(), test_handle()).await;
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

**The box parses nothing.** It sends what was typed, verbatim, as
`FromBrowser::Send { id, text }`, and the server parses it with the one parser
from Task 1 — so the room and the terminal can never come to disagree about
what `/json` means. The panel lists the roster's `skills` so a person can see
which `/<skill>` commands exist, and shows a `ToBrowser::Refused` as a line in
the transcript, which is what `/clear`, `/exit`, `/quit` and `/bye` get.

An agent that disappears is greyed when `ToBrowser::Left` arrives and removed
by a 60-second timer in the page, cancelled if a `Joined` with the same id
arrives first. The server sends no second frame; Task 7 says why.

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

`tmux new-session -d -s jarvo-<name> -c <cwd> <exe> --office-dir <dir>
--office-name <name>`. The name has to be passed: without it the new Jarvo
names itself after its directory and the name chosen in the room is silently
dropped.

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
    assert!(
        argv.windows(2).any(|w| w[0] == "--office-name" && w[1] == "api"),
        "the name chosen in the room reaches the new Jarvo"
    );
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

- What the office is and how to turn it on (`--office-dir` /
  `RIGAGENT_OFFICE_DIR`), what `--office-name` is for, and that `--office-bind`
  is the single flag that makes the office reachable from another machine.
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
