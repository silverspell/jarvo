# The Office: many Jarvos on one screen

A design for a shared control surface over several running `rigagent`
instances. Written 2026-10-25, against `642d0fb` plus the branch that adds
Maestro.

## Intent

Today a Jarvo is a single terminal: one person, one agent, one screen. The
interesting work is already elsewhere — several agents running at once on a
machine that is not the one in front of you, each on its own errand.

The Office is a web page that shows every running Jarvo as a character in a
room, and lets you click one and tell it what to do. It is not a monitor
hanging on the wall: you drive from it. A remote box runs N Jarvos; you open
one URL and work them all.

The why, in the person's own framing: run this on a remote server, start as
many Jarvos as the work needs, see them in one place, and later share their
sessions so a conversation can be picked up from somewhere else.

Shape of the thing, as decided:

```text
browser  ──HTTP+WS──▶  office server  ◀──WS──  jarvo #1 (TUI, tmux)
                       (own process)  ◀──WS──  jarvo #2
                                      ◀──WS──  jarvo #3
                              │
                              └── spawns a new jarvo on request
```

## Goals

- One page shows every live Jarvo, what it is called, and what it is doing
  right now: idle, thinking, running a named tool, waiting.
- Clicking an agent opens its live transcript beside the room, with a command
  box. Typing there reaches that agent's conversation exactly as if it had
  been typed into its own TUI.
- A command sent to a busy agent is **queued**, visibly, and runs when the
  current turn finishes.
- The office can **start a new Jarvo**: pick a directory, a new agent appears
  in the room.
- The first Jarvo to start brings the office up by itself. There is no
  separate "start the server first" step.
- The office outlives any single Jarvo. Closing the one that happened to start
  it changes nothing for the others.
- Reachable from a browser over the network, protected by a JWT, with no
  cookies.

## Non-goals

- **Session sharing.** Picking up another agent's conversation elsewhere is
  wanted later and should not be designed out, but nothing here implements it.
- **Steering a turn in flight.** Considered and dropped: Rig's multi-turn
  stream has no hook between tool calls, so the honest version aborts and
  restarts the turn, losing what it already did. Queueing is enough.
- **Replacing the TUI.** Each Jarvo keeps its terminal interface. A headless
  mode is a plausible follow-on once the bridge exists, not part of this.
- **Pixel Agents compatibility.** Evaluated, then rejected in favour of our own
  server; see below.
- Multi-user accounts, roles, or an audit trail.

## Anti-goals

Things that would make this a failure even if every box were ticked:

- **A second way to lose the terminal.** Nothing in the office may write to a
  Jarvo's stdout or stderr, or make its TUI stutter. The interface must stay
  off the model's path, and now off the network's path too.
- **An office that must be running.** A Jarvo whose office is missing,
  unreachable, or broken must start normally and work normally, exactly as it
  does today when an MCP server fails. The office is opt-in in the same way the
  shell and MCP are.
- **A control surface wearing a monitor's clothes.** If the room looks lovely
  and you still have to ssh in to actually tell an agent something, this failed.
- **Silent reach.** Anyone who can open the office can run shell commands on
  that machine through an agent that has the shell enabled. If that is not
  obvious from the design and the documentation, this failed.

## Constraints

From `AGENTS.md`, which governs this repository:

- **Nothing writes to the terminal while the TUI runs.** Logging is file-only,
  gated on `RIGAGENT_LOG_FILE`. A new server component must respect this in
  both directions: inside a Jarvo it logs through `tracing`; as its own process
  it owns its stdout and may print.
- **The interface never blocks on the model**, and now must never block on a
  socket either. The office connection is a third party to the existing
  `AgentCommand` / `AgentEvent` channels, not a layer in front of them.
- **Never fake a capability.** If the office cannot do something — reach an
  agent, start one, show a transcript — it says so.
- **Prefer a plain function over a new trait.** This codebase deliberately has
  no abstraction layer over Rig; the office should not introduce one over the
  transport either.
- **Shell access stays opt-in**, and the office does not get to change that.
  Whether a spawned Jarvo has a shell is a property of how it is started.
- `src/tui/app.rs` holds no I/O. Whatever the office needs from the interface
  does not go there.

Facts about the current code that bear on the design:

- `agent::start` (`src/agent.rs:145-236`) returns an `AgentHandle` (a clone of
  an `mpsc::Sender<AgentCommand>`) and an `mpsc::Receiver<AgentEvent>`. The
  handle is cheap to clone; the receiver is single-consumer. This asymmetry is
  the central problem to solve (see Open questions).
- The command channel already buffers 256 messages, and the agent task does not
  call `recv` while a turn is running. **The queue already exists.** What does
  not exist is the interface admitting it: `submit` rejects a second message
  with "still working on the previous message" (`src/tui/mod.rs:241-244`).
- `AgentEvent` already carries everything the room needs: `Started`, `Delta`,
  `ToolCall { name }`, `ToolResult { name }`, `Notice`, `ContextTokens`,
  `Completed`, `Failed` (`src/agent.rs:86-104`).
- Blank path options are deliberately permitted and mean "off"
  (`any_path` / `non_empty_path` in `src/config.rs`). A new path option must
  follow that pattern or an empty `.env` line stops the program from starting.

## Decisions already made

These were settled in conversation and are not open:

1. **Our own server, not Pixel Agents.** Pixel Agents (the VS Code extension
   and CLI at `pixel-agents-hq/pixel-agents`) solves the adjacent problem: it
   visualises Claude Code sessions from hook events, and its "send input" story
   is an embedded terminal. Using it would mean a Node process under a Rust
   binary and Jarvo pretending to be Claude Code. We write our own and own the
   protocol. Its registration pattern — each server writes a file under
   `~/.pixel-agents/servers/` so clients can find it — is worth copying.
2. **The office server is its own process.** The first Jarvo spawns it detached;
   every Jarvo, including that first one, then connects as an ordinary client.
   No Jarvo is special, and quitting one never takes the office down.
3. **Layout: room plus inspector.** The room is the larger part of the page;
   selecting an agent opens a panel beside it with that agent's live transcript
   and a command box. The room should stay roomy — the panel must not squeeze
   it into a strip. Pixel art is a later coat of paint over this layout.
4. **Queued, not rejected, not interrupted.** A command for a busy agent waits
   its turn, and both the office and the TUI show that something is waiting.
5. **The office can start a new Jarvo.** Choose a directory, get an agent.
6. **JWT, no cookies.** Reached directly from a browser over the network, not
   through an ssh tunnel.
7. **The TUI stays.** Every Jarvo keeps its terminal interface, including the
   ones nobody is looking at.
8. **TLS comes from a reverse proxy, not from us.** The office speaks plain
   HTTP and is meant to sit behind Caddy, nginx, or an equivalent when it is
   reachable from the network. We do not take on certificates. The
   documentation has to say plainly that exposing the bare port sends the
   bearer token and the whole conversation in cleartext.
9. **The command box takes prompts, `/json` and skill commands — not the
   destructive ones.** `/clear` and the quit commands do not cross the
   network. Nobody wipes a conversation or closes a Jarvo somebody else is
   sitting in front of from the room. The parsing that today lives in
   `tui/mod.rs::submit` therefore has to be shared, minus those two cases.
10. **The inspector streams deltas live.** Text appears as the model writes it,
    the way it does in the terminal. This is what makes the room a control
    surface rather than a status board, and it is the reason the exposure note
    above matters: the whole conversation crosses the network continuously.
11. **tmux is a given on the host.** A Jarvo the office starts runs in a named
    tmux session, so a person can `ssh` in and attach to the same screen. No
    pty of our own.

## Decisions made by the author of this document

Called out so they can be overruled while reading:

- **Token issuance is Jupyter-shaped.** The server generates a signing key on
  first run and keeps it under its state directory; it prints a URL carrying a
  JWT when it starts. No password screen, no user accounts. The browser keeps
  the token in `localStorage` (not a cookie) and sends it as
  `Authorization: Bearer` on HTTP and in the WebSocket handshake. Anyone with
  the token has the office.
- **Default bind is `127.0.0.1`.** Reaching it from another machine is a
  deliberate flag, and the documentation says plainly what that exposes.
- **An agent's name** comes from an explicit option if given, otherwise the
  basename of its working directory, with a short suffix when that collides.
- **The server keeps no history on disk.** It holds a bounded tail of each
  agent's transcript in memory so a browser that connects late sees context.
  Durable sessions belong to the session-sharing work that is out of scope here.
- **Nothing in the office is a tool the model can call.** The office is for the
  person, not the agent; no new tool is registered on the agent.
- **The office is opt-in per Jarvo, by flag.** Off unless a Jarvo is started
  with the office option (and its blank-means-off `.env` form, following
  `any_path`/`non_empty_path`). A bare `rigagent` does not suddenly spawn a
  detached server or listen on a port. Among the Jarvos that *do* have it on,
  the first one still brings the office up by itself: the goal was no separate
  "start the server first" step, not "on by default".
- **A prompt from the browser is echoed in the terminal.** Otherwise an answer
  appears in the TUI with no question above it. Both surfaces also show what is
  waiting, not merely that something is. The channel's spare capacity is not
  enough for that — an `mpsc::Sender` knows how many slots are free, not what
  sits in them — so the pending work has to be visible somewhere both surfaces
  can read. Which mechanism (a new `AgentEvent` variant, or a queue the agent
  task owns and publishes) is the builder's call, and it is upstream of the
  fan-out question below.
- **One office per user, per machine.** The registration lives in the user's
  own state directory. Two people on the same box get two offices on two ports
  and cannot reach each other's agents — which keeps "silent reach" bounded to
  one account. Jarvos on several machines sharing one room is out of scope.
- **A restarted Jarvo is a new character.** Identity lasts for the life of a
  process. An agent that disappears greys out, stays in the room for a short
  while so you can see it went, then drops.
- **The wire format is built so session sharing can land later.** Stable turn
  identifiers and a transcript shape that can be persisted verbatim, from the
  first version. Nothing persists them yet. This is cheap now and expensive to
  retrofit once the format is in use.

## What is left to the builder

- The transport and its crates. Something like `axum` (its WebSocket support is
  built in) plus `tokio-tungstenite` for the Jarvo-side client is the obvious
  fit for a Tokio application, but the choice, the versions, and whether the
  server lives in this crate behind a subcommand or beside it are the builder's.
  Note that this binary is currently dependency-light and has no HTTP stack at
  all; adding one is a real change in the project's weight and worth stating
  plainly in the plan.
- The wire format. It must carry agent identity, the `AgentEvent` variants,
  queue depth, and the commands going the other way. Whether that is JSON over
  one WebSocket per Jarvo, or something else, is open.
- How a Jarvo finds the office and how exactly the first one wins the race to
  start it. A registration file under a state directory, as Pixel Agents does,
  plus an atomic lock is the suggested shape; the race is the part to get right,
  since several Jarvos may start in the same second.
- What a spawned Jarvo inherits: which directory, which provider and model,
  whether it gets a shell, and how it is supervised once started. **Whether the
  office may spawn an agent with `run_shell` enabled is a safety decision, not
  a detail** — it needs an explicit answer in the plan.
- Everything about how the room looks and animates, within the layout above.
- Where the office's own code goes in the module table, and what `AGENTS.md`
  needs to say about it afterwards.

## Open questions

Carried deliberately rather than guessed:

- **Fan-out of `AgentEvent`.** The receiver is single-consumer and the TUI
  consumes it. The office needs the same events. Two candidates: the TUI
  forwards each event to the office bridge as it handles it (small, keeps
  ownership obvious, but makes the interface responsible for someone else's
  feed); or `AgentEvent` moves to a `tokio::sync::broadcast` so both subscribe
  (cleaner, but a lagging subscriber drops events and every existing consumer
  changes). This is the single most consequential unresolved choice, and it
  should be settled with a spike rather than an argument.
- **What "queued" looks like in the TUI.** The office and the terminal now
  share one queue. The terminal currently refuses a second message; it must
  instead accept it and show the depth. Exactly how it reads in a 80-column
  footer that is already budgeted (`AGENTS.md` warns a test asserts this) is
  unresolved.
- **Reconnection.** What a Jarvo does when the office dies underneath it — and
  which of them brings it back up — is unresolved. What the room shows for an
  agent that stops answering is settled above.
