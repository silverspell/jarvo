//! A Jarvo's side of the office: starts or finds the server, then keeps its
//! socket open. Every failure here is a note in the conversation, never an
//! error that could keep Jarvo from starting.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use super::protocol::{AgentInfo, FromAgent, ToAgent, TurnEvent, TurnId};
use super::registry::{self, Registration};
use crate::agent::{AgentCommand, AgentEvent, AgentHandle, Origin};

/// How long a freshly spawned server gets to publish its registration.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the WebSocket connect and Hello may take before the office is
/// treated as broken and the agent carries on.
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the registration is checked while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Events waiting for the socket. `EventSink` drops rather than blocks, so a
/// full channel costs frames, not the agent.
const EVENT_CAPACITY: usize = 256;

/// Join the office at `dir`, starting the server if nobody has.
///
/// Returns the channel the agent's events should also go to, plus a note for
/// the conversation. Any failure is a note, never an error.
pub async fn join(
    dir: &Path,
    bind: &str,
    info: AgentInfo,
    handle: AgentHandle,
) -> (Option<mpsc::Sender<AgentEvent>>, String) {
    let dir = match super::absolute_dir(dir) {
        Ok(dir) => dir,
        Err(err) => {
            return (
                None,
                format!("Office: cannot resolve {} ({err})", dir.display()),
            );
        }
    };
    let registration = match acquire(&dir, bind).await {
        Ok(registration) => registration,
        Err(problem) => return (None, format!("Office: {problem}")),
    };

    let url = agent_url(&registration);
    let joined = tokio::time::timeout(JOIN_TIMEOUT, async {
        let (socket, _response) = connect_async(&url).await.map_err(|err| {
            // The URL carries the token, so the problem names host and port.
            format!(
                "cannot connect to the server on {}:{} ({err})",
                registration.host, registration.port
            )
        })?;
        let (mut sink, stream) = socket.split();
        let hello = serde_json::to_string(&FromAgent::Hello { info })
            .map_err(|_| "cannot encode the hello message".to_string())?;
        sink.send(Message::Text(hello.into()))
            .await
            .map_err(|err| {
                format!(
                    "the server on {}:{} closed the connection ({err})",
                    registration.host, registration.port
                )
            })?;
        Ok::<_, String>((sink, stream))
    })
    .await;

    let (mut sink, mut stream) = match joined {
        Ok(Ok(joined)) => joined,
        Ok(Err(problem)) => return (None, format!("Office: {problem}")),
        Err(_) => {
            return (
                None,
                format!(
                    "Office: the server on {}:{} did not answer within {} seconds",
                    registration.host,
                    registration.port,
                    JOIN_TIMEOUT.as_secs()
                ),
            );
        }
    };

    let (events, mut outgoing) = mpsc::channel(EVENT_CAPACITY);
    tokio::spawn(async move {
        let mut turn = TurnId(0);
        loop {
            tokio::select! {
                event = outgoing.recv() => {
                    let Some(event) = event else { break };
                    let Some(message) = to_wire(event, &mut turn) else { continue };
                    let Ok(text) = serde_json::to_string(&message) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                incoming = stream.next() => {
                    let text = match incoming {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(_)) => continue,
                    };
                    let Ok(message) = serde_json::from_str::<ToAgent>(text.as_str()) else {
                        continue;
                    };
                    if deliver(&handle, message).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    (Some(events), room_url(&registration))
}

/// The agent socket address, using the host the server advertised.
fn agent_url(registration: &Registration) -> String {
    format!(
        "ws://{}:{}/ws/agent?token={}",
        registration.host, registration.port, registration.token
    )
}

/// The room address shown in the conversation. It carries the token and is
/// never logged.
fn room_url(registration: &Registration) -> String {
    format!(
        "http://{}:{}/?token={}",
        registration.host, registration.port, registration.token
    )
}

/// Find the office, or become the one that starts it.
async fn acquire(dir: &Path, bind: &str) -> Result<Registration, String> {
    if let Some(registration) = registry::find(dir) {
        return Ok(registration);
    }

    match registry::claim(dir) {
        // This process won the claim and owes the room a server.
        Ok(true) => start_server(dir, bind)?,
        // Someone else is starting it; the registration is what tells us when.
        Ok(false) => {}
        Err(err) => return Err(format!("cannot use {} ({err})", dir.display())),
    }

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(registration) = registry::find(dir) {
            return Ok(registration);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the server for {} did not start within {} seconds",
                dir.display(),
                STARTUP_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Start the office server as its own process.
///
/// `Stdio::null()` on all three is deliberate: a child inherits them by
/// default, so its startup line — or its panic — would print onto the TUI.
fn start_server(dir: &Path, bind: &str) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot find the Jarvo executable ({err})"))?;
    let mut command = Command::new(exe);
    command
        .arg("--serve-office")
        .arg(dir)
        .arg("--office-bind")
        .arg(bind)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group: closing the terminal that started it must not
    // SIGHUP the office out from under every other Jarvo.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|err| format!("cannot start the server ({err})"))?;
    // Reap it off the async task so it does not linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Turn one agent event into a wire frame, assigning it to the current turn.
///
/// The turn id belongs to this side: a `Started` opens the next one, and an
/// `Accepted` is TUI-only and never crosses the wire. `Notice` and friends may
/// arrive before any turn, hence [`TurnId(0)`](TurnId).
fn to_wire(event: AgentEvent, turn: &mut TurnId) -> Option<FromAgent> {
    let event = match event {
        AgentEvent::Started { prompt } => {
            turn.0 += 1;
            TurnEvent::Started { prompt }
        }
        AgentEvent::Accepted { .. } => return None,
        AgentEvent::Queued { text, depth } => TurnEvent::Queued { text, depth },
        AgentEvent::Delta(text) => TurnEvent::Delta { text },
        AgentEvent::ToolCall { name } => TurnEvent::ToolCall { name },
        AgentEvent::ToolResult { name } => TurnEvent::ToolResult { name },
        AgentEvent::Notice(text) => TurnEvent::Notice { text },
        AgentEvent::ContextTokens(tokens) => TurnEvent::ContextTokens { tokens },
        AgentEvent::Completed { text } => TurnEvent::Completed { text },
        AgentEvent::Failed(message) => TurnEvent::Failed { message },
    };
    Some(FromAgent::Turn {
        turn: turn.clone(),
        event,
    })
}

/// A prompt from the room becomes an agent command, marked as the office's.
async fn deliver(handle: &AgentHandle, message: ToAgent) -> anyhow::Result<()> {
    let command = match message {
        ToAgent::Prompt { text } => AgentCommand::Prompt {
            text,
            origin: Origin::Office,
        },
        ToAgent::Structured { text } => AgentCommand::Structured {
            text,
            origin: Origin::Office,
        },
        ToAgent::Skill { name, request } => AgentCommand::Skill {
            name,
            request,
            origin: Origin::Office,
        },
    };
    handle.send(command).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentCommand, AgentHandle, Origin};
    use crate::office::protocol::{AgentId, TurnEvent};
    use tokio::sync::mpsc;

    fn test_info() -> AgentInfo {
        AgentInfo {
            id: AgentId("test".into()),
            name: "tester".into(),
            model: "test:model".into(),
            cwd: "/tmp".into(),
            skills: vec![],
        }
    }

    fn test_handle() -> (AgentHandle, mpsc::Receiver<AgentCommand>) {
        let (tx, rx) = mpsc::channel(4);
        (AgentHandle::with_sender(tx), rx)
    }

    #[tokio::test]
    async fn an_office_that_cannot_be_claimed_is_a_note_not_a_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("not-a-directory");
        std::fs::write(&path, b"").expect("write");

        let (sink, note) = join(&path, "127.0.0.1:0", test_info(), test_handle().0).await;
        assert!(sink.is_none());
        assert!(
            note.to_lowercase().contains("office"),
            "the note names the problem: {note}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_never_answers_is_a_note_not_a_hang() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let port = listener.local_addr().expect("address").port();
        tokio::spawn(async move {
            // Accept and never complete the WebSocket handshake.
            let mut held = Vec::new();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                held.push(socket);
            }
        });

        let dir = tempfile::tempdir().expect("temp dir");
        let registration = Registration {
            host: "127.0.0.1".into(),
            port,
            pid: std::process::id(),
            token: "t".into(),
        };
        registry::publish(dir.path(), &registration).expect("publish");
        assert!(
            registry::find(dir.path()).is_some(),
            "the registration looks live"
        );

        let (sink, note) = join(dir.path(), "127.0.0.1:0", test_info(), test_handle().0).await;
        assert!(sink.is_none(), "a wedged office is not joined");
        assert!(
            note.contains("did not answer"),
            "the note names the timeout: {note}"
        );
    }

    #[test]
    fn the_urls_use_the_advertised_host() {
        let registration = Registration {
            host: "192.168.1.10".into(),
            port: 8080,
            pid: 1,
            token: "token".into(),
        };
        assert_eq!(
            agent_url(&registration),
            "ws://192.168.1.10:8080/ws/agent?token=token"
        );
        assert_eq!(
            room_url(&registration),
            "http://192.168.1.10:8080/?token=token"
        );
    }

    #[tokio::test]
    async fn a_prompt_from_the_office_reaches_the_agent_with_its_origin() {
        let (handle, mut commands) = test_handle();

        deliver(&handle, ToAgent::Prompt { text: "hi".into() })
            .await
            .expect("the command should reach the handle");

        match commands.recv().await.expect("a command") {
            AgentCommand::Prompt { text, origin } => {
                assert_eq!(text, "hi");
                assert_eq!(origin, Origin::Office);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn turn_ids_advance_on_started_and_skip_accepted() {
        let mut turn = TurnId(0);

        let Some(FromAgent::Turn { turn: id, event }) =
            to_wire(AgentEvent::Notice("hello".into()), &mut turn)
        else {
            panic!("a notice is sent");
        };
        assert_eq!(id, TurnId(0));
        assert_eq!(
            event,
            TurnEvent::Notice {
                text: "hello".into()
            }
        );

        let Some(FromAgent::Turn { turn: id, event }) =
            to_wire(AgentEvent::Started { prompt: "q".into() }, &mut turn)
        else {
            panic!("a start is sent");
        };
        assert_eq!(id, TurnId(1));
        assert_eq!(event, TurnEvent::Started { prompt: "q".into() });

        for event in [
            AgentEvent::Delta("d".into()),
            AgentEvent::Queued {
                text: "q2".into(),
                depth: 1,
            },
        ] {
            let Some(FromAgent::Turn { turn: id, .. }) = to_wire(event, &mut turn) else {
                panic!("a mid-turn event is sent");
            };
            assert_eq!(id, TurnId(1), "mid-turn events carry the current turn");
        }

        assert!(
            to_wire(AgentEvent::Accepted { text: "q".into() }, &mut turn).is_none(),
            "accepted never crosses the wire"
        );

        let Some(FromAgent::Turn { turn: id, .. }) =
            to_wire(AgentEvent::Completed { text: "a".into() }, &mut turn)
        else {
            panic!("a completion is sent");
        };
        assert_eq!(id, TurnId(1));

        let Some(FromAgent::Turn { turn: id, .. }) = to_wire(
            AgentEvent::Started {
                prompt: "q3".into(),
            },
            &mut turn,
        ) else {
            panic!("a second start is sent");
        };
        assert_eq!(id, TurnId(2));
    }

    #[tokio::test]
    async fn a_joined_jarvo_trades_turns_with_the_room() {
        use crate::office::protocol::{FromBrowser, ToBrowser};
        use crate::office::{registry, server};

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();
        tokio::spawn(async move {
            let _ = server::serve(&path, "127.0.0.1:0").await;
        });

        let registration = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(registration) = registry::find(dir.path()) {
                    return registration;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the server registers");

        let (handle, mut commands) = test_handle();
        let (extra, note) = join(dir.path(), "127.0.0.1:0", test_info(), handle.clone()).await;
        let extra = extra.expect("the office is joined");
        assert!(
            note.starts_with(&format!("http://127.0.0.1:{}/?token=", registration.port)),
            "the note carries the room's address: {note}"
        );

        let browser_url = format!(
            "ws://127.0.0.1:{}/ws/browser?token={}",
            registration.port, registration.token
        );
        let (browser, _) = connect_async(&browser_url)
            .await
            .expect("the browser connects");
        let (mut browser_sink, mut browser_stream) = browser.split();

        // Hello reaches the room asynchronously; wait for it to be listed.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let Some(Ok(Message::Text(text))) = browser_stream.next().await else {
                    panic!("the browser socket closed");
                };
                let Ok(message) = serde_json::from_str::<ToBrowser>(text.as_str()) else {
                    continue;
                };
                match message {
                    ToBrowser::Roster { agents } if agents.contains(&test_info()) => break,
                    ToBrowser::Joined { info } if info == test_info() => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("the room lists the agent");

        let send = FromBrowser::Send {
            id: test_info().id,
            text: "from room".into(),
        };
        browser_sink
            .send(Message::Text(
                serde_json::to_string(&send).expect("encode").into(),
            ))
            .await
            .expect("the browser sends");

        let command = tokio::time::timeout(Duration::from_secs(2), commands.recv())
            .await
            .expect("a command arrives")
            .expect("a command");
        match command {
            AgentCommand::Prompt { text, origin } => {
                assert_eq!(text, "from room");
                assert!(origin == Origin::Office);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        extra
            .send(AgentEvent::Started {
                prompt: "hello".into(),
            })
            .await
            .expect("the extra channel accepts an event");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let Some(Ok(Message::Text(text))) = browser_stream.next().await else {
                    panic!("the browser socket closed");
                };
                let Ok(message) = serde_json::from_str::<ToBrowser>(text.as_str()) else {
                    continue;
                };
                if let ToBrowser::Turn {
                    id, turn, event, ..
                } = message
                    && id == test_info().id
                {
                    assert_eq!(turn, TurnId(1));
                    let started =
                        matches!(&event, TurnEvent::Started { prompt } if prompt == "hello");
                    assert!(started, "unexpected event: {event:?}");
                    break;
                }
            }
        })
        .await
        .expect("the turn crosses the room");
    }
}
