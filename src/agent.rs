//! Conversation handling: agent construction, streaming, tools and history.
//!
//! The agent runs in its own Tokio task. The TUI sends it [`AgentCommand`]s and
//! reads [`AgentEvent`]s back over channels, so rendering never blocks on the
//! model and the model never touches the terminal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use futures::StreamExt;
use rig::agent::{CompletionCall, MultiTurnStreamItem};
use rig::completion::Message;
use rig::extractor::{Extractor, ExtractorBuilder};
use rig::streaming::{Item, StreamEvent};
use rig::{Agent, AgentBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::docs::{Docs, ReadSkill};
use crate::mcp;
use crate::providers;
use crate::tools::{GetCurrentTime, RunShell};

/// Capacity of both channels between the TUI and the agent task. Deltas arrive
/// faster than frames are drawn, so the buffer is generous.
const CHANNEL_CAPACITY: usize = 256;

/// Added to the extractor's own preamble, which is phrased for pulling data out
/// of a supplied document. Here the text is a request to answer first.
const STRUCTURED_INSTRUCTIONS: &str = "The provided text is the user's question or request. \
Answer it yourself using your own knowledge, then submit that answer through the \
`submit` function: a short `title`, a one- or two-sentence `summary`, the \
supporting `key_points` in order of importance, and a `confidence` between 0.0 \
and 1.0 reporting how sure you are of the answer. Be honest about low \
confidence rather than defaulting to a high value.";

/// A structured answer, used by the `/json` command.
///
/// This is the Serde type the model's response is deserialized into; its JSON
/// schema is what the model is asked to fill in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AnswerCard {
    /// A short title for the answer.
    pub title: String,
    /// One- or two-sentence summary.
    pub summary: String,
    /// The supporting points, most important first.
    pub key_points: Vec<String>,
    /// How sure the model is of the answer, from 0.0 to 1.0.
    ///
    /// Self-reported, so treat it as a hint rather than a measurement.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub confidence: f32,
}

impl AnswerCard {
    /// Whether [`confidence`](Self::confidence) is inside the documented range.
    ///
    /// The schema asks for 0.0 to 1.0, but a model can return anything, so the
    /// interface checks rather than trusts.
    pub fn confidence_in_range(&self) -> bool {
        (0.0..=1.0).contains(&self.confidence)
    }
}

/// What the TUI asks the agent to do.
#[derive(Debug)]
pub enum AgentCommand {
    /// Answer `prompt` as streamed prose.
    Prompt(String),
    /// Answer `prompt` as a JSON [`AnswerCard`].
    Structured(String),
    /// Run the skill named `name`, with `request` as what the user wants from
    /// it. The body is read by the agent task, which owns the library.
    Skill { name: String, request: String },
    /// Forget the conversation so far.
    Reset,
}

/// What the agent reports back while working.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A turn has begun.
    Started,
    /// A fragment of assistant text.
    Delta(String),
    /// The model called a tool.
    ToolCall { name: String },
    /// A tool returned.
    ToolResult { name: String },
    /// An informational line for the chat panel.
    Notice(String),
    /// How many tokens the model's last request carried, prompt and reply
    /// together: what the next turn starts from.
    ContextTokens(u64),
    /// The turn finished; the full assistant text is attached.
    Completed { text: String },
    /// The turn failed. The conversation is left as it was.
    Failed(String),
}

/// The TUI's end of the agent task.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    commands: mpsc::Sender<AgentCommand>,
}

impl AgentHandle {
    /// Queue a command for the agent.
    ///
    /// Fails only once the agent task is gone.
    pub async fn send(&self, command: AgentCommand) -> Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| anyhow::anyhow!("the agent task has stopped"))
    }

    /// A handle backed by `commands`, so the interface can be tested without
    /// a live agent task behind it.
    #[cfg(test)]
    pub fn with_sender(commands: mpsc::Sender<AgentCommand>) -> Self {
        Self { commands }
    }
}

/// A started agent: the handle to drive it, the event feed, and notes about
/// optional components (such as MCP) gathered during startup.
pub struct Started {
    pub handle: AgentHandle,
    pub events: mpsc::Receiver<AgentEvent>,
    pub notes: Vec<String>,
    /// Skill names, so the interface can offer them as `/name`.
    pub skills: Vec<String>,
}

/// Build the agent and spawn the task that owns it.
///
/// MCP servers named in the config are started here; a failure to reach one is
/// returned in [`Started::notes`] and does not prevent startup.
pub async fn start(config: &Config) -> Result<Started> {
    let model = providers::build_model(config)?;
    let mcp_tools = mcp::load(config.mcp_config.as_deref().map(Path::new)).await;
    let mut notes = mcp_tools.notes.clone();
    if mcp_tools.is_empty() {
        tracing::debug!("no MCP tools; running with native tools only");
    }

    let skills = Docs::load(config.skills_dir.as_deref());
    notes.extend(skills.notes.iter().cloned());
    if skills.is_empty() {
        tracing::debug!("no skill library; running with the bare system prompt");
    } else {
        tracing::info!(skills = skills.len(), "skill library loaded");
    }

    // Only the catalogue goes into the preamble; bodies are opened on demand
    // through read_skill, so a large library costs a few hundred tokens a turn
    // instead of its full weight.
    let mut preamble = config.system_prompt.clone();
    if let Some(index) = skills.index() {
        preamble.push_str(&index);
    }

    // `/json` goes through Rig's extractor, which offers the schema as a
    // `submit` output tool. Unlike a native structured-output constraint, that
    // works on any tool-capable model, DeepSeek included. It needs its own
    // agent because the extraction preamble and tool choice replace the chat
    // agent's, so the model is cloned rather than rebuilt.
    let extractor = ExtractorBuilder::<AnswerCard>::new(model.clone())
        .append_preamble(STRUCTURED_INSTRUCTIONS)
        .retries(1)
        .build();

    let mut builder = AgentBuilder::new(model)
        .name("rigagent")
        .preamble(&preamble)
        .tool(GetCurrentTime)
        .tool(ReadSkill::new(skills.clone()))
        .dynamic_tools(mcp_tools.tools);

    // Shell access is opt-in and announced, because nothing confirms a
    // command before it runs.
    match shell_dir(config.shell_dir.as_deref()) {
        Ok(Some(dir)) => {
            notes.push(format!("Shell: commands run in {}", dir.display()));
            builder = builder.tool(RunShell::new(dir));
        }
        Ok(None) => tracing::debug!("no shell directory; run_shell not registered"),
        Err(note) => notes.push(note),
    }

    let agent = builder.build();

    let (command_tx, command_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(CHANNEL_CAPACITY);

    let task = AgentTask {
        agent,
        skills: skills.clone(),
        extractor,
        history: Vec::new(),
        max_turns: config.max_turns,
        // Held only to keep the MCP connections (and so their tools) alive for
        // as long as the agent exists.
        _mcp: mcp_tools.connections,
    };
    tokio::spawn(task.run(command_rx, event_tx));

    Ok(Started {
        handle: AgentHandle {
            commands: command_tx,
        },
        events: event_rx,
        notes,
        skills: skills.ids(),
    })
}

/// The agent and the conversation it owns.
struct AgentTask {
    agent: Agent,
    /// The skill library, for `/name` invocations. The agent's `read_skill`
    /// tool holds its own copy for the model's own lookups.
    skills: Docs,
    extractor: Extractor<AnswerCard>,
    history: Vec<Message>,
    max_turns: usize,
    _mcp: Vec<rig::tool::rmcp::McpConnection>,
}

impl AgentTask {
    /// Serve commands until the TUI drops its handle.
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<AgentCommand>,
        events: mpsc::Sender<AgentEvent>,
    ) {
        while let Some(command) = commands.recv().await {
            match command {
                AgentCommand::Reset => {
                    self.history.clear();
                    let _ = events
                        .send(AgentEvent::Notice("conversation cleared".into()))
                        .await;
                }
                AgentCommand::Prompt(prompt) => {
                    if events.send(AgentEvent::Started).await.is_err() {
                        return;
                    }
                    let event = self.stream_turn(&prompt, &events).await;
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
                AgentCommand::Structured(prompt) => {
                    if events.send(AgentEvent::Started).await.is_err() {
                        return;
                    }
                    let event = self.structured_turn(&prompt, &events).await;
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
                AgentCommand::Skill { name, request } => {
                    if events.send(AgentEvent::Started).await.is_err() {
                        return;
                    }
                    let event = match self.skills.body(&name) {
                        Ok(body) => {
                            let prompt = skill_prompt(&body, &request);
                            self.stream_turn(&prompt, &events).await
                        }
                        Err(err) => AgentEvent::Failed(err.to_string()),
                    };
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// One streamed turn: forward deltas and tool activity as they happen, and
    /// commit the turn to the conversation only once it succeeds.
    async fn stream_turn(&mut self, prompt: &str, events: &mpsc::Sender<AgentEvent>) -> AgentEvent {
        let mut stream = self
            .agent
            .prompt(prompt)
            .history(self.history.iter().cloned())
            .max_turns(self.max_turns)
            .stream();

        // Tool results carry the call id, not the name; remember the pairing so
        // the status line can name the tool that just returned.
        let mut call_names: HashMap<String, String> = HashMap::new();
        let mut text = String::new();

        while let Some(item) = stream.next().await {
            let item = match item {
                Ok(item) => item,
                Err(err) => return AgentEvent::Failed(err.to_string()),
            };

            match item {
                MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text {
                    text: delta,
                    ..
                })) => {
                    text.push_str(&delta);
                    if events.send(AgentEvent::Delta(delta)).await.is_err() {
                        return AgentEvent::Failed("the interface stopped listening".into());
                    }
                }
                MultiTurnStreamItem::ToolCall { tool_call } => {
                    let name = tool_call.function.name.to_string();
                    call_names.insert(tool_call.id.to_string(), name.clone());
                    let _ = events.send(AgentEvent::ToolCall { name }).await;
                }
                MultiTurnStreamItem::ToolResult { tool_result } => {
                    let name = call_names
                        .get(&tool_result.call.to_string())
                        .cloned()
                        .unwrap_or_else(|| "tool".to_string());
                    let _ = events.send(AgentEvent::ToolResult { name }).await;
                }
                MultiTurnStreamItem::ModelTurnRetried { .. } => {
                    // The deltas so far were provisional; drop them and let the
                    // interface start the assistant message over.
                    text.clear();
                    let _ = events
                        .send(AgentEvent::Notice("model turn retried".into()))
                        .await;
                }
                MultiTurnStreamItem::FinalResponse(response) => {
                    if let Some(tokens) = context_tokens(&response.completion_calls) {
                        let _ = events.send(AgentEvent::ContextTokens(tokens)).await;
                    }
                    // `messages` holds the prompt and everything the turn
                    // committed, so the conversation grows by exactly one turn.
                    self.history.extend(response.messages.iter().cloned());
                    let output = response.output();
                    let text = if output.is_empty() { text } else { output };
                    return AgentEvent::Completed { text };
                }
                _ => {}
            }
        }

        AgentEvent::Failed("the model stream ended without a response".into())
    }

    /// One structured turn: the model fills in [`AnswerCard`]'s schema through
    /// the `submit` output tool, and the arguments are deserialized before they
    /// reach the interface.
    ///
    /// This is a single turn with the output tool forced, so the agent's own
    /// tools are not available here.
    async fn structured_turn(
        &mut self,
        prompt: &str,
        events: &mpsc::Sender<AgentEvent>,
    ) -> AgentEvent {
        let response = self
            .extractor
            .extract(prompt)
            .history(self.history.iter().cloned())
            .await;

        match response {
            Ok(response) => {
                // The gauge is deliberately not updated here. The extractor
                // is a separate run carrying only the `submit` tool, so its
                // request is far smaller than the chat agent's and reporting
                // it would make the context look as though it had shrunk.
                // The figure from the last chat turn stays until the next one.
                if !response.output.confidence_in_range() {
                    let confidence = response.output.confidence;
                    tracing::warn!(confidence, "model reported confidence outside 0.0..=1.0");
                    let _ = events
                        .send(AgentEvent::Notice(format!(
                            "note: reported confidence {confidence} is outside 0.0–1.0"
                        )))
                        .await;
                }
                let text = match serde_json::to_string_pretty(&response.output) {
                    Ok(text) => text,
                    Err(err) => {
                        return AgentEvent::Failed(format!("could not render the answer: {err}"));
                    }
                };
                // Record a plain text exchange rather than the extractor's own
                // messages: those carry a `submit` tool call that the chat
                // agent does not advertise, and replaying it would describe a
                // tool the next turn does not have.
                self.history.push(Message::user(prompt));
                self.history.push(Message::assistant(text.clone()));
                AgentEvent::Completed { text }
            }
            Err(err) => AgentEvent::Failed(err.to_string()),
        }
    }
}

/// The turn a `/skill` invocation sends.
///
/// The body goes in as the user's message rather than the preamble so it
/// applies to this turn and stays in the conversation afterwards, which is
/// what lets a follow-up question refer back to the work it produced.
fn skill_prompt(body: &str, request: &str) -> String {
    // Saying the body is already here matters: the preamble tells the model to
    // open a matching skill with read_skill, and without this it may spend a
    // tool call re-fetching what it was just handed.
    let mut prompt = String::from(
        "Follow the instructions in the skill below for this request. Its full \
         text is included here, so there is no need to open it with \
         read_skill.\n\n----- skill -----\n",
    );
    prompt.push_str(body.trim());
    prompt.push_str("\n----- end of skill -----\n\n");
    if request.is_empty() {
        prompt.push_str("The user invoked this skill with no further request.");
    } else {
        prompt.push_str("The request: ");
        prompt.push_str(request);
    }
    prompt
}

/// Tokens the run's last request carried: its prompt plus the reply it drew.
///
/// That sum is what the next turn's prompt starts from, which is what the
/// usage gauge reports. `None` when the provider reported no counters, which
/// is normal for Ollama and for any model that omits usage from a stream.
fn context_tokens(calls: &[CompletionCall]) -> Option<u64> {
    let usage = calls.last()?.usage;
    let input = usage.input_tokens?;
    Some(input + usage.output_tokens.unwrap_or(0))
}

/// Resolve the shell working directory, if shell access is configured.
///
/// `Ok(None)` means no shell was asked for. An `Err` carries a note for the
/// conversation: a bad path leaves the agent running without the tool rather
/// than failing to start, like the rest of the optional machinery here.
fn shell_dir(configured: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let Some(dir) = configured else {
        return Ok(None);
    };
    // Canonicalised so the tool description and the log name one real path,
    // not whatever relative spelling reached the configuration.
    match dir.canonicalize() {
        Ok(path) if path.is_dir() => Ok(Some(path)),
        Ok(path) => Err(format!(
            "Shell: {} is not a directory, so run_shell is off",
            path.display()
        )),
        Err(err) => Err(format!(
            "Shell: cannot use {} ({err}), so run_shell is off",
            dir.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialize a model reply into [`AnswerCard`], as the `/json` path does.
    fn parse_answer_card(text: &str) -> Result<AnswerCard> {
        use anyhow::Context as _;
        serde_json::from_str(text).context("response is not a valid AnswerCard")
    }

    #[test]
    fn answer_card_deserializes_from_model_json() {
        let card = parse_answer_card(
            r#"{
                 "title": "Ownership",
                 "summary": "Rust tracks who owns each value.",
                 "key_points": ["one owner at a time", "borrows are checked"],
                 "confidence": 0.9
               }"#,
        )
        .expect("a well-formed card should deserialize");

        assert_eq!(card.title, "Ownership");
        assert_eq!(card.key_points.len(), 2);
        assert_eq!(card.confidence, 0.9);
        assert!(card.confidence_in_range());
    }

    #[test]
    fn answer_card_round_trips() {
        let card = AnswerCard {
            title: "Streaming".into(),
            summary: "Tokens arrive incrementally.".into(),
            key_points: vec!["low latency".into()],
            confidence: 0.75,
        };
        let json = serde_json::to_string(&card).expect("serialize");
        assert_eq!(parse_answer_card(&json).expect("deserialize"), card);
    }

    #[test]
    fn answer_card_rejects_a_missing_field() {
        assert!(parse_answer_card(r#"{ "title": "No summary" }"#).is_err());
        assert!(
            parse_answer_card(r#"{ "title": "t", "summary": "s", "key_points": [] }"#).is_err(),
            "confidence is required"
        );
    }

    #[test]
    fn an_out_of_range_confidence_is_detected_not_rejected() {
        // A model can return anything; the card still parses and the interface
        // flags the value instead of failing the turn.
        let card = parse_answer_card(
            r#"{ "title": "t", "summary": "s", "key_points": [], "confidence": 95 }"#,
        )
        .expect("the card should still deserialize");

        assert!(!card.confidence_in_range());
    }

    #[test]
    fn answer_card_has_an_object_schema() {
        let schema = serde_json::to_value(schemars::schema_for!(AnswerCard))
            .expect("the schema should serialize");
        assert_eq!(schema["type"], "object");
        assert!(
            schema["properties"].get("key_points").is_some(),
            "schema should describe key_points: {schema}"
        );
        let confidence = &schema["properties"]["confidence"];
        assert_eq!(
            confidence["minimum"], 0.0,
            "schema should bound confidence: {schema}"
        );
        assert_eq!(
            confidence["maximum"], 1.0,
            "schema should bound confidence: {schema}"
        );
    }
}
