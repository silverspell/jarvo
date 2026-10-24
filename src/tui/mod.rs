//! Terminal lifecycle, input events and the main loop.
//!
//! The loop owns the terminal and nothing else: agent work happens in a
//! separate task ([`crate::agent`]) and reaches here as [`AgentEvent`]s over a
//! Tokio channel, so streaming never blocks key handling.

pub mod app;
pub mod ui;

use anyhow::{Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::agent::{self, AgentCommand, AgentEvent};
use crate::config::Config;
use app::{App, Role, Status};

/// Terminal-event channel depth. Key presses are drained every frame, so this
/// only has to absorb a burst such as a paste.
const INPUT_CAPACITY: usize = 128;

/// Start the agent, take over the terminal, and run until the user quits.
///
/// The terminal is restored before this returns, including on error.
pub async fn run(config: Config) -> Result<()> {
    let started = agent::start(&config).await?;

    let mut terminal = init()?;
    let result = event_loop(
        &mut terminal,
        App::new(
            config.header_label(),
            started.notes,
            config.context_window,
            started.skills,
        ),
        started.handle,
        started.events,
    )
    .await;
    restore();

    result
}

/// Enter the alternate screen and raw mode.
///
/// A panic hook is installed so a crash inside the loop still leaves a usable
/// terminal behind, and keyboard enhancement flags are requested so terminals
/// that support them can report Shift+Enter distinctly.
fn init() -> Result<DefaultTerminal> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));

    let terminal = ratatui::try_init().context("failed to initialize the terminal")?;

    if matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) {
        // Best effort: without this, Shift+Enter is indistinguishable from
        // Enter and Alt+Enter is the newline key.
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    Ok(terminal)
}

/// Undo [`init`]. Safe to call more than once, and from a panic hook.
fn restore() {
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    ratatui::restore();
}

/// Read terminal events on a blocking thread and forward them.
///
/// `crossterm`'s reader is blocking, so it gets a thread of its own rather than
/// a Tokio worker.
fn spawn_input_reader() -> mpsc::Receiver<Event> {
    let (sender, receiver) = mpsc::channel(INPUT_CAPACITY);
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                // The loop has exited and dropped the receiver.
                Ok(event) => {
                    if sender.blocking_send(event).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    tracing::error!(error = %err, "terminal input closed");
                    break;
                }
            }
        }
    });
    receiver
}

/// Draw, wait for input or agent output, repeat.
async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut app: App,
    handle: agent::AgentHandle,
    mut events: mpsc::Receiver<AgentEvent>,
) -> Result<()> {
    let mut input = spawn_input_reader();

    loop {
        terminal
            .draw(|frame| ui::draw(frame, &mut app))
            .context("failed to draw the interface")?;

        tokio::select! {
            event = input.recv() => {
                let Some(event) = event else {
                    // The reader thread is gone; there is no way to drive the
                    // interface any more.
                    return Ok(());
                };
                if let Event::Key(key) = event {
                    handle_key(&mut app, key, &handle).await?;
                }
            }
            event = events.recv() => {
                let Some(event) = event else {
                    return Ok(());
                };
                app.apply_event(event);
                // Deltas arrive far faster than frames; fold whatever else is
                // already queued into this one redraw.
                while let Ok(event) = events.try_recv() {
                    app.apply_event(event);
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

/// Input that closes the application, each equivalent to Ctrl+C.
const QUIT_COMMANDS: [&str; 3] = ["/quit", "/exit", "/bye"];

/// Apply one key press, sending a command to the agent when a message is
/// submitted.
async fn handle_key(app: &mut App, key: KeyEvent, handle: &agent::AgentHandle) -> Result<()> {
    // Windows reports press and release; act on presses only.
    if key.kind == KeyEventKind::Release {
        return Ok(());
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Char('c') | KeyCode::Char('d') if ctrl => app.should_quit = true,
        // Ctrl+J is the portable newline: terminals send it even without
        // keyboard enhancement, where Shift+Enter looks exactly like Enter.
        KeyCode::Char('j') if ctrl => app.insert_newline(),
        KeyCode::Enter if alt || shift => app.insert_newline(),
        KeyCode::Enter => submit(app, handle).await?,

        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Home => app.move_line_start(),
        KeyCode::End if ctrl => app.scroll_to_bottom(),
        KeyCode::End => app.move_line_end(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),

        KeyCode::Char(ch) if !ctrl => app.insert_char(ch),
        _ => {}
    }

    Ok(())
}

/// The skill `text` invokes, if its first word names one.
///
/// Checked against the loaded library rather than a pattern, so an unknown
/// `/command` still reaches the model as ordinary text instead of silently
/// doing nothing.
fn skill_command(text: &str, skills: &[String]) -> Option<String> {
    let rest = text.strip_prefix('/')?;
    let name = rest.split_whitespace().next()?;
    skills.iter().find(|skill| skill.as_str() == name).cloned()
}

/// Send the input buffer to the agent, honouring the `/` commands.
async fn submit(app: &mut App, handle: &agent::AgentHandle) -> Result<()> {
    // Checked before the busy guard, and before the input is consumed, so a
    // quit command behaves exactly like Ctrl+C: it works mid-turn. A run that
    // has gone astray must never trap the user in the interface.
    if QUIT_COMMANDS.contains(&app.input.trim()) {
        app.should_quit = true;
        return Ok(());
    }
    if app.busy {
        app.status = Status::Error("still working on the previous message".into());
        return Ok(());
    }
    let Some(text) = app.take_input() else {
        return Ok(());
    };

    let command = if let Some(rest) = text.strip_prefix("/json") {
        let prompt = rest.trim();
        if prompt.is_empty() {
            app.push(Role::System, "usage: /json <prompt>");
            return Ok(());
        }
        app.push(Role::User, text.clone());
        app.push(
            Role::System,
            "requesting a structured answer (not streamed)",
        );
        AgentCommand::Structured(prompt.to_string())
    } else if let Some(name) = skill_command(&text, &app.skills) {
        let request = text[1 + name.len()..].trim().to_string();
        app.push(Role::User, text.clone());
        app.push(Role::System, format!("running skill: {name}"));
        AgentCommand::Skill { name, request }
    } else if text == "/clear" {
        app.entries.clear();
        app.status = Status::Idle;
        AgentCommand::Reset
    } else {
        app.push(Role::User, text.clone());
        AgentCommand::Prompt(text)
    };

    handle.send(command).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app and a handle whose commands the test can inspect.
    fn fixture() -> (App, agent::AgentHandle, mpsc::Receiver<AgentCommand>) {
        let (tx, rx) = mpsc::channel(4);
        (
            App::new(
                "deepseek:deepseek-chat".into(),
                Vec::new(),
                64_000,
                vec!["teach".to_string()],
            ),
            agent::AgentHandle::with_sender(tx),
            rx,
        )
    }

    fn type_in(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.insert_char(ch);
        }
    }

    #[tokio::test]
    async fn every_quit_command_closes_the_application() {
        for command in QUIT_COMMANDS {
            let (mut app, handle, mut rx) = fixture();
            type_in(&mut app, command);
            submit(&mut app, &handle).await.expect("submit should work");

            assert!(app.should_quit, "{command} should quit");
            assert!(
                rx.try_recv().is_err(),
                "{command} should not reach the agent"
            );
        }
    }

    #[tokio::test]
    async fn quitting_works_mid_turn_like_ctrl_c() {
        let (mut app, handle, _rx) = fixture();
        app.busy = true;
        type_in(&mut app, "  /exit  ");

        submit(&mut app, &handle).await.expect("submit should work");

        assert!(app.should_quit, "a stuck run must not trap the user");
        assert!(
            !matches!(app.status, Status::Error(_)),
            "quitting is not the busy-guard error path"
        );
    }

    #[tokio::test]
    async fn a_slash_command_naming_a_skill_runs_it() {
        let (mut app, handle, mut rx) = fixture();
        type_in(&mut app, "/teach me about borrow checking");

        submit(&mut app, &handle).await.expect("submit should work");

        match rx.try_recv() {
            Ok(AgentCommand::Skill { name, request }) => {
                assert_eq!(name, "teach");
                assert_eq!(request, "me about borrow checking");
            }
            other => panic!("expected a skill command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_skill_may_be_invoked_with_no_request() {
        let (mut app, handle, mut rx) = fixture();
        type_in(&mut app, "/teach");

        submit(&mut app, &handle).await.expect("submit should work");

        assert!(
            matches!(rx.try_recv(), Ok(AgentCommand::Skill { name, request })
                if name == "teach" && request.is_empty())
        );
    }

    #[tokio::test]
    async fn an_unknown_slash_command_is_sent_as_text() {
        let (mut app, handle, mut rx) = fixture();
        type_in(&mut app, "/nosuchskill do a thing");

        submit(&mut app, &handle).await.expect("submit should work");

        assert!(
            matches!(rx.try_recv(), Ok(AgentCommand::Prompt(text))
                if text == "/nosuchskill do a thing"),
            "an unknown command must not silently do nothing"
        );
    }

    #[tokio::test]
    async fn an_ordinary_message_still_reaches_the_agent() {
        let (mut app, handle, mut rx) = fixture();
        type_in(&mut app, "/exitplan the migration");

        submit(&mut app, &handle).await.expect("submit should work");

        assert!(!app.should_quit, "only an exact command quits");
        assert!(
            matches!(rx.try_recv(), Ok(AgentCommand::Prompt(text)) if text == "/exitplan the migration")
        );
    }
}
