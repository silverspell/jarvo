//! Application state: the transcript, the input buffer, status and scrolling.
//!
//! Nothing here touches the terminal or the network; it is a plain state
//! machine driven by key presses and [`AgentEvent`]s, which makes it testable.

use crate::agent::AgentEvent;

/// Who produced a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// Application messages: notices, tool activity, errors.
    System,
}

impl Role {
    /// The prefix shown in the chat panel.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::User => "you",
            Self::Assistant => "assistant",
            Self::System => "·",
        }
    }
}

/// One entry in the transcript.
#[derive(Debug, Clone)]
pub struct Entry {
    pub role: Role,
    pub text: String,
}

/// What the agent is doing, shown in the status area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Generating,
    /// A tool is running.
    Tool(String),
    Error(String),
}

impl Status {
    /// The status text.
    pub fn text(&self) -> String {
        match self {
            Self::Idle => "idle".to_string(),
            Self::Generating => "generating…".to_string(),
            Self::Tool(name) => format!("running tool: {name}"),
            Self::Error(message) => format!("error: {message}"),
        }
    }
}

/// How full the context window is, as one of five equal fifths.
///
/// Fair weather means room to spare; a storm means the conversation is about
/// to outgrow the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weather {
    /// Under a fifth used.
    Sunny,
    PartlyCloudy,
    Overcast,
    Rain,
    /// Four fifths or more used.
    Storm,
}

impl Weather {
    /// The fifth `percent` falls in. 0–19 is sunny, 80–100 a storm.
    pub fn for_percent(percent: u16) -> Self {
        match percent / 20 {
            0 => Self::Sunny,
            1 => Self::PartlyCloudy,
            2 => Self::Overcast,
            3 => Self::Rain,
            // 100% lands on 5, and anything beyond the assumed window above it.
            _ => Self::Storm,
        }
    }

    /// The glyph shown in the footer.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Sunny => "\u{2600}",
            Self::PartlyCloudy => "\u{26c5}",
            Self::Overcast => "\u{2601}",
            Self::Rain => "\u{1f327}",
            Self::Storm => "\u{26c8}",
        }
    }
}

/// The whole interface state.
pub struct App {
    /// `provider:model`, shown in the header.
    pub header: String,
    pub entries: Vec<Entry>,
    /// The multiline input buffer. May contain `\n`.
    pub input: String,
    /// Cursor position in [`input`](Self::input), as a byte offset on a char
    /// boundary.
    pub cursor: usize,
    pub status: Status,
    /// Index of the assistant entry this turn is streaming into. Cleared when
    /// the turn ends, so the next turn starts a fresh entry of its own.
    draft: Option<usize>,
    /// First visible transcript line.
    pub scroll: u16,
    /// Whether the view sticks to the newest output.
    pub follow: bool,
    /// Set while a turn is in flight; input is accepted but not sent.
    pub busy: bool,
    /// Set once the user asks to quit.
    pub should_quit: bool,
    /// Transcript height of the last frame, used by PageUp/PageDown.
    pub viewport_height: u16,
    /// Total wrapped transcript lines in the last frame.
    pub content_height: u16,
    /// Tokens the model's last request carried, or `None` before the first
    /// reply and whenever the provider reports no usage.
    pub context_tokens: Option<u64>,
    /// The context window those tokens are measured against. An assumption
    /// from the provider or `--context-window`, not a reported limit.
    pub context_limit: u64,
    /// Skill names, so `/name` can be told apart from an unknown command.
    pub skills: Vec<String>,
}

impl App {
    /// A fresh interface for `header`, with `notes` shown as system entries.
    pub fn new(
        header: String,
        notes: Vec<String>,
        context_limit: u64,
        skills: Vec<String>,
    ) -> Self {
        let mut entries = vec![Entry {
            role: Role::System,
            text: "Enter sends · Alt+Enter (or Shift+Enter) for a newline · \
                   /json for structured output · /clear to reset · \
                   /exit, /bye, /quit or Ctrl+C quits"
                .to_string(),
        }];
        entries.extend(notes.into_iter().map(|text| Entry {
            role: Role::System,
            text,
        }));

        Self {
            header,
            entries,
            input: String::new(),
            cursor: 0,
            status: Status::Idle,
            draft: None,
            scroll: 0,
            follow: true,
            busy: false,
            should_quit: false,
            viewport_height: 0,
            content_height: 0,
            context_tokens: None,
            context_limit: context_limit.max(1),
            skills,
        }
    }

    /// How much of the assumed context window the last request used, rounded
    /// down, capped at 100. `None` until a provider reports usage.
    pub fn context_percent(&self) -> Option<u16> {
        let tokens = self.context_tokens?;
        let percent = tokens.saturating_mul(100) / self.context_limit.max(1);
        Some(u16::try_from(percent).unwrap_or(100).min(100))
    }

    // ---- input editing -------------------------------------------------

    /// Insert `ch` at the cursor.
    pub fn insert_char(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Insert a line break at the cursor.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if let Some(previous) = self.prev_boundary() {
            self.input.remove(previous);
            self.cursor = previous;
        }
    }

    /// Delete the character at the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    /// Move the cursor one character left.
    pub fn move_left(&mut self) {
        if let Some(previous) = self.prev_boundary() {
            self.cursor = previous;
        }
    }

    /// Move the cursor one character right.
    pub fn move_right(&mut self) {
        if let Some(next) = self.input[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    /// Move the cursor to the start of the current line.
    pub fn move_line_start(&mut self) {
        self.cursor = self.input[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
    }

    /// Move the cursor to the end of the current line.
    pub fn move_line_end(&mut self) {
        self.cursor = self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |offset| self.cursor + offset);
    }

    /// The byte offset of the character before the cursor, if any.
    fn prev_boundary(&self) -> Option<usize> {
        self.input[..self.cursor]
            .chars()
            .next_back()
            .map(|ch| self.cursor - ch.len_utf8())
    }

    /// Take the trimmed input buffer, leaving it empty.
    ///
    /// Returns `None` when there is nothing to send.
    pub fn take_input(&mut self) -> Option<String> {
        let text = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        (!text.is_empty()).then_some(text)
    }

    // ---- transcript ----------------------------------------------------

    /// Append an entry.
    ///
    /// Follow mode is left alone: output arriving while the user is reading
    /// further up must not yank the view back down. Only an explicit act —
    /// sending a message, paging to the bottom, Ctrl+End — resumes following.
    pub fn push(&mut self, role: Role, text: impl Into<String>) {
        self.entries.push(Entry {
            role,
            text: text.into(),
        });
    }

    /// The assistant entry in progress, if the draft index still points at one.
    ///
    /// The check keeps a stale index from writing into a different entry after
    /// the transcript is cleared mid-turn.
    fn draft_entry(&self) -> Option<usize> {
        let index = self.draft?;
        match self.entries.get(index) {
            Some(entry) if entry.role == Role::Assistant => Some(index),
            _ => None,
        }
    }

    /// Append `delta` to the assistant entry in progress, starting one if
    /// there is none. Intervening user or system entries are left alone.
    pub fn push_delta(&mut self, delta: &str) {
        let index = self.draft_entry().unwrap_or_else(|| {
            self.entries.push(Entry {
                role: Role::Assistant,
                text: String::new(),
            });
            self.draft = Some(self.entries.len() - 1);
            self.entries.len() - 1
        });
        self.entries[index].text.push_str(delta);
    }

    // ---- scrolling -----------------------------------------------------

    /// The largest useful scroll offset for the last frame's geometry.
    pub fn max_scroll(&self) -> u16 {
        self.content_height.saturating_sub(self.viewport_height)
    }

    /// Scroll up by one screenful, leaving follow mode.
    pub fn page_up(&mut self) {
        self.scroll_up(self.viewport_height.max(1));
    }

    /// Scroll up by `lines`, leaving follow mode.
    pub fn scroll_up(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_sub(lines);
        self.follow = false;
    }

    /// Scroll down by `lines`, re-entering follow mode at the bottom.
    pub fn scroll_down(&mut self, lines: u16) {
        let max = self.max_scroll();
        self.scroll = self.scroll.saturating_add(lines).min(max);
        if self.scroll >= max {
            self.follow = true;
        }
    }

    /// Scroll down by one screenful, re-entering follow mode at the bottom.
    pub fn page_down(&mut self) {
        self.scroll_down(self.viewport_height.max(1));
    }

    /// Jump to the newest output.
    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
        self.scroll = self.max_scroll();
    }

    // ---- agent events --------------------------------------------------

    /// Fold one agent event into the state.
    pub fn apply_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Started { .. } => {
                self.busy = true;
                self.status = Status::Generating;
                self.draft = None;
            }
            AgentEvent::Queued { text, depth } => {
                self.push(Role::System, format!("queued ({depth}): {text}"));
            }
            AgentEvent::Accepted { text } => self.push(Role::User, text),
            AgentEvent::Delta(delta) => {
                self.status = Status::Generating;
                self.push_delta(&delta);
            }
            AgentEvent::ToolCall { name } => {
                self.status = Status::Tool(name.clone());
                self.push(Role::System, format!("→ {name}"));
            }
            AgentEvent::ToolResult { name } => {
                self.status = Status::Generating;
                self.push(Role::System, format!("← {name}"));
            }
            AgentEvent::Notice(text) => self.push(Role::System, text),
            AgentEvent::ContextTokens(tokens) => self.context_tokens = Some(tokens),
            AgentEvent::Completed { text } => {
                self.busy = false;
                self.status = Status::Idle;
                // A non-streaming turn (structured output) produces no deltas,
                // so the text may not be on screen yet. Writing through the
                // draft keeps a queued turn's entries from being overwritten.
                match self.draft_entry() {
                    Some(index) => self.entries[index].text = text,
                    None if !text.is_empty() => self.push(Role::Assistant, text),
                    None => {}
                }
                self.draft = None;
            }
            AgentEvent::Failed(message) => {
                self.busy = false;
                self.draft = None;
                self.status = Status::Error(message.clone());
                self.push(Role::System, format!("error: {message}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new("openai:gpt-4o-mini".into(), Vec::new(), 1_000, Vec::new())
    }

    #[test]
    fn typing_and_editing_tracks_the_cursor() {
        let mut app = app();
        for ch in "héllo".chars() {
            app.insert_char(ch);
        }
        assert_eq!(app.input, "héllo");
        assert_eq!(app.cursor, app.input.len());

        app.move_left();
        app.move_left();
        app.backspace();
        assert_eq!(app.input, "hélo");

        app.move_line_start();
        assert_eq!(app.cursor, 0);
        app.delete();
        assert_eq!(app.input, "élo");

        app.move_line_end();
        assert_eq!(app.cursor, app.input.len());
    }

    #[test]
    fn multibyte_navigation_stays_on_char_boundaries() {
        let mut app = app();
        app.insert_char('ö');
        app.move_left();
        assert_eq!(app.cursor, 0);
        app.move_right();
        assert_eq!(app.cursor, 2, "ö is two bytes");
    }

    #[test]
    fn newlines_are_inserted_without_sending() {
        let mut app = app();
        app.insert_char('a');
        app.insert_newline();
        app.insert_char('b');
        assert_eq!(app.input, "a\nb");

        app.move_line_start();
        assert_eq!(
            app.cursor, 2,
            "the cursor goes to the start of the second line"
        );
    }

    #[test]
    fn take_input_trims_and_clears() {
        let mut app = app();
        for ch in "  hi  ".chars() {
            app.insert_char(ch);
        }
        assert_eq!(app.take_input().as_deref(), Some("hi"));
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
        assert_eq!(app.take_input(), None, "blank input sends nothing");
    }

    #[test]
    fn deltas_accumulate_into_one_assistant_entry() {
        let mut app = app();
        app.push(Role::User, "hello");
        app.apply_event(AgentEvent::Delta("Hi".into()));
        app.apply_event(AgentEvent::Delta(" there".into()));

        let last = app.entries.last().expect("an entry");
        assert_eq!(last.role, Role::Assistant);
        assert_eq!(last.text, "Hi there");
    }

    #[test]
    fn a_tool_call_shows_in_the_status_and_transcript() {
        let mut app = app();
        app.apply_event(AgentEvent::ToolCall {
            name: "get_current_time".into(),
        });
        assert_eq!(app.status, Status::Tool("get_current_time".into()));
        assert!(app.status.text().contains("get_current_time"));

        app.apply_event(AgentEvent::ToolResult {
            name: "get_current_time".into(),
        });
        assert_eq!(app.status, Status::Generating);
    }

    #[test]
    fn a_queued_message_is_announced_and_an_office_prompt_is_echoed() {
        let mut app = app();
        app.apply_event(AgentEvent::Queued {
            text: "later".into(),
            depth: 1,
        });
        assert!(app.entries.last().expect("entry").text.contains("later"));

        app.apply_event(AgentEvent::Accepted {
            text: "from the room".into(),
        });
        let last = app.entries.last().expect("entry");
        assert_eq!(last.role, Role::User);
        assert_eq!(last.text, "from the room");
    }

    #[test]
    fn a_queued_turn_gets_its_own_assistant_entry() {
        let mut app = app();
        app.push(Role::User, "q1");
        app.apply_event(AgentEvent::Started {
            prompt: "q1".into(),
        });
        app.apply_event(AgentEvent::Delta("par".into()));

        // What `submit` and the agent do while the first turn is still running.
        app.push(Role::User, "q2");
        app.apply_event(AgentEvent::Queued {
            text: "q2".into(),
            depth: 1,
        });
        app.apply_event(AgentEvent::Delta("tial".into()));
        app.apply_event(AgentEvent::Completed {
            text: "first answer".into(),
        });

        app.apply_event(AgentEvent::Started {
            prompt: "q2".into(),
        });
        app.apply_event(AgentEvent::Delta("second".into()));
        app.apply_event(AgentEvent::Completed {
            text: "second answer".into(),
        });

        let answers: Vec<&str> = app
            .entries
            .iter()
            .filter(|entry| entry.role == Role::Assistant)
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(answers, ["first answer", "second answer"]);
    }

    #[test]
    fn clearing_the_transcript_mid_turn_starts_a_fresh_draft() {
        let mut app = app();
        app.apply_event(AgentEvent::Started {
            prompt: "q1".into(),
        });
        app.apply_event(AgentEvent::Delta("partial".into()));

        // `/clear` empties the vector; the in-flight turn keeps streaming.
        app.entries.clear();
        app.apply_event(AgentEvent::Delta("after clear".into()));
        app.apply_event(AgentEvent::Completed {
            text: "done".into(),
        });

        let answers: Vec<&str> = app
            .entries
            .iter()
            .filter(|entry| entry.role == Role::Assistant)
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(answers, ["done"]);
    }

    #[test]
    fn a_turn_starts_busy_and_ends_idle() {
        let mut app = app();
        app.apply_event(AgentEvent::Started {
            prompt: "hello".into(),
        });
        assert!(app.busy);
        assert_eq!(app.status, Status::Generating);

        app.apply_event(AgentEvent::Delta("partial".into()));
        app.apply_event(AgentEvent::Completed {
            text: "partial answer".into(),
        });
        assert!(!app.busy);
        assert_eq!(app.status, Status::Idle);
        assert_eq!(
            app.entries.last().expect("an entry").text,
            "partial answer",
            "the final text replaces the streamed draft"
        );
    }

    #[test]
    fn a_non_streaming_turn_still_shows_its_answer() {
        let mut app = app();
        app.push(Role::User, "/json hello");
        app.apply_event(AgentEvent::Started {
            prompt: "hello".into(),
        });
        app.apply_event(AgentEvent::Completed {
            text: "{\n  \"title\": \"Hi\"\n}".into(),
        });

        let last = app.entries.last().expect("an entry");
        assert_eq!(last.role, Role::Assistant);
        assert!(last.text.contains("title"));
    }

    #[test]
    fn a_failure_is_reported_and_clears_busy() {
        let mut app = app();
        app.apply_event(AgentEvent::Started {
            prompt: "hello".into(),
        });
        app.apply_event(AgentEvent::Failed("no credentials".into()));

        assert!(!app.busy);
        assert_eq!(app.status, Status::Error("no credentials".into()));
        assert!(
            app.entries
                .last()
                .expect("an entry")
                .text
                .contains("no credentials")
        );
    }

    #[test]
    fn paging_clamps_and_toggles_follow() {
        let mut app = app();
        app.viewport_height = 10;
        app.content_height = 25;
        app.scroll = 15;

        app.page_up();
        assert_eq!(app.scroll, 5);
        assert!(!app.follow, "scrolling up stops following");

        app.page_up();
        assert_eq!(app.scroll, 0, "scrolling stops at the top");

        app.page_down();
        assert_eq!(app.scroll, 10);
        assert!(!app.follow);

        app.page_down();
        assert_eq!(app.scroll, 15, "scrolling stops at the bottom");
        assert!(app.follow, "reaching the bottom resumes following");
    }

    #[test]
    fn line_scrolling_clamps_at_both_ends() {
        let mut app = app();
        app.viewport_height = 10;
        app.content_height = 25;

        app.scroll_down(3);
        assert_eq!(app.scroll, 3);
        app.scroll_up(1);
        assert_eq!(app.scroll, 2);
        app.scroll_up(99);
        assert_eq!(app.scroll, 0, "scrolling stops at the top");
        assert!(!app.follow);

        app.scroll_down(u16::MAX);
        assert_eq!(app.scroll, 15, "scrolling stops at the bottom");
        assert!(app.follow, "reaching the bottom resumes following");
    }

    #[test]
    fn output_arriving_while_scrolled_back_leaves_the_view_alone() {
        let mut app = app();
        app.viewport_height = 10;
        app.content_height = 50;
        app.page_up();
        assert!(!app.follow);
        let resting = app.scroll;

        // A whole turn's worth of events: tool calls, streamed text and the
        // completion. None of it may drag the reader back down.
        app.apply_event(AgentEvent::ToolCall {
            name: "run_shell".into(),
        });
        app.apply_event(AgentEvent::Delta("a long answer".into()));
        app.apply_event(AgentEvent::Completed {
            text: "a long answer, finished".into(),
        });

        assert!(!app.follow, "the view stays where the reader put it");
        assert_eq!(app.scroll, resting);
    }

    #[test]
    fn sending_a_message_resumes_following() {
        let mut app = app();
        app.viewport_height = 10;
        app.content_height = 50;
        app.page_up();
        assert!(!app.follow);

        app.scroll_to_bottom();
        assert!(app.follow);
        assert_eq!(app.scroll, app.max_scroll());
    }

    #[test]
    fn the_window_splits_into_five_equal_fifths() {
        let fifths = [
            (0, Weather::Sunny),
            (19, Weather::Sunny),
            (20, Weather::PartlyCloudy),
            (39, Weather::PartlyCloudy),
            (40, Weather::Overcast),
            (59, Weather::Overcast),
            (60, Weather::Rain),
            (79, Weather::Rain),
            (80, Weather::Storm),
            (100, Weather::Storm),
        ];
        for (percent, expected) in fifths {
            assert_eq!(
                Weather::for_percent(percent),
                expected,
                "{percent}% belongs to a different fifth"
            );
        }
    }

    #[test]
    fn usage_is_unknown_until_a_provider_reports_it() {
        let mut app = app();
        assert_eq!(app.context_percent(), None, "nothing is known up front");

        // The helper's window is 1_000 tokens.
        app.apply_event(AgentEvent::ContextTokens(150));
        assert_eq!(app.context_percent(), Some(15));
        assert_eq!(Weather::for_percent(15), Weather::Sunny);

        app.apply_event(AgentEvent::ContextTokens(905));
        assert_eq!(app.context_percent(), Some(90));
        assert_eq!(Weather::for_percent(90), Weather::Storm);
    }

    #[test]
    fn usage_past_the_assumed_window_caps_at_a_hundred() {
        let mut app = app();
        app.apply_event(AgentEvent::ContextTokens(4_000));
        assert_eq!(
            app.context_percent(),
            Some(100),
            "an underestimated window must not overflow the gauge"
        );
    }

    #[test]
    fn short_transcripts_do_not_scroll() {
        let mut app = app();
        app.viewport_height = 10;
        app.content_height = 3;
        assert_eq!(app.max_scroll(), 0);

        app.page_down();
        assert_eq!(app.scroll, 0);
    }
}
