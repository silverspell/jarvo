//! Native Rust tools the agent can call.
//!
//! Each tool is an ordinary function annotated with `#[rig_tool]`, which
//! generates the tool type, its parameter schema and its `Tool` implementation.

use chrono::Utc;
use rig::rig_tool;
use rig::tool::ToolExecutionError;
use serde::{Deserialize, Serialize};

/// What [`get_current_time`] hands back to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentTime {
    /// The current UTC instant, RFC 3339 with a `Z` offset.
    pub utc: String,
    /// The same instant as seconds since the Unix epoch.
    pub unix_seconds: i64,
}

/// Returns the current UTC timestamp.
#[rig_tool(
    name = "get_current_time",
    description = "Get the current date and time in UTC. Takes no arguments."
)]
pub fn get_current_time() -> Result<CurrentTime, ToolExecutionError> {
    let now = Utc::now();
    Ok(CurrentTime {
        utc: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        unix_seconds: now.timestamp(),
    })
}

/// How much of each output stream reaches the model, in bytes.
///
/// A command like `find /` would otherwise bury the conversation; the result
/// says when it has been cut rather than trimming silently.
const MAX_STREAM_BYTES: usize = 24_000;

/// Default seconds a command may run before it is killed.
const DEFAULT_TIMEOUT_SECONDS: u64 = 120;

/// Ceiling on the timeout a caller may ask for.
const MAX_TIMEOUT_SECONDS: u64 = 900;

/// Arguments to [`RunShell`].
#[derive(Debug, Deserialize)]
pub struct RunShellArgs {
    /// The command line, interpreted by `sh -c`.
    pub command: String,
    /// Seconds before the command is killed. Defaults to
    /// [`DEFAULT_TIMEOUT_SECONDS`], capped at [`MAX_TIMEOUT_SECONDS`].
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// What [`RunShell`] hands back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellOutput {
    /// The process exit code, or `None` if it was killed by a signal or the
    /// timeout.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Set when either stream was cut at [`MAX_STREAM_BYTES`].
    pub truncated: bool,
    /// Set when the command was killed for running too long.
    pub timed_out: bool,
}

/// Runs a shell command in a fixed working directory.
///
/// This is general shell access: whatever `sh -c` accepts, it runs, with the
/// permissions of the process. There is no allow-list and no confirmation
/// step, so the working directory is the only boundary, and even that only
/// bounds where commands *start* — a command is free to name an absolute path
/// elsewhere. It is off unless `--shell-dir` is given.
#[derive(Debug, Clone)]
pub struct RunShell {
    dir: std::path::PathBuf,
}

impl RunShell {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self { dir }
    }
}

/// What can go wrong before a command produces output.
#[derive(Debug)]
pub struct ShellSpawnError(std::io::Error);

impl std::fmt::Display for ShellSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not start the command: {}", self.0)
    }
}

impl std::error::Error for ShellSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl rig::tool::Tool for RunShell {
    const NAME: &'static str = "run_shell";

    type Args = RunShellArgs;
    type Output = ShellOutput;
    type Error = ShellSpawnError;

    fn description(&self) -> String {
        format!(
            "Run a shell command with `sh -c` in {} and return its exit code, \
             stdout and stderr. Use it for git, builds, tests and other CLI \
             tools. Commands run without confirmation, so read before you \
             delete or overwrite. Output over {MAX_STREAM_BYTES} bytes per \
             stream is truncated, and a command is killed after \
             {DEFAULT_TIMEOUT_SECONDS}s unless timeout_seconds says otherwise. \
             There is no stdin: a command that waits for input will fail \
             rather than hang.",
            self.dir.display()
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command line, interpreted by `sh -c`.",
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description":
                        "Seconds before the command is killed. Defaults to 120, capped at 900.",
                    "minimum": 1,
                },
            },
            "required": ["command"],
        })
    }

    fn annotations(&self) -> rig::completion::ToolAnnotations {
        rig::completion::ToolAnnotations {
            title: Some("Run shell command".to_string()),
            // Everything else here is a hint to a host that enforces one.
            // This application does not: the honest value is that a shell
            // command may do anything at all.
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        }
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let seconds = args
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
            .clamp(1, MAX_TIMEOUT_SECONDS);

        // Logged in full: with no confirmation step, the log is the only
        // record of what the model actually ran.
        tracing::info!(command = %args.command, dir = %self.dir.display(), "running shell command");

        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&self.dir)
            // No stdin, so anything that prompts fails instead of hanging.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Dropping the future on timeout must take the process with it.
            .kill_on_drop(true)
            .spawn()
            .map_err(ShellSpawnError)?;

        let duration = std::time::Duration::from_secs(seconds);
        match tokio::time::timeout(duration, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let (stdout, out_cut) = clip(&output.stdout);
                let (stderr, err_cut) = clip(&output.stderr);
                tracing::info!(exit = ?output.status.code(), "shell command finished");
                Ok(ShellOutput {
                    exit_code: output.status.code(),
                    stdout,
                    stderr,
                    truncated: out_cut || err_cut,
                    timed_out: false,
                })
            }
            Ok(Err(err)) => Err(ShellSpawnError(err)),
            Err(_) => {
                tracing::warn!(seconds, command = %args.command, "shell command timed out");
                Ok(ShellOutput {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("killed after {seconds}s"),
                    truncated: false,
                    timed_out: true,
                })
            }
        }
    }
}

/// Decode `bytes` as text, cut to [`MAX_STREAM_BYTES`] on a char boundary.
///
/// Returns the text and whether anything was dropped.
fn clip(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_STREAM_BYTES {
        return (text.into_owned(), false);
    }
    let mut end = MAX_STREAM_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = text.len() - end;
    (
        format!("{}\n[... {dropped} more bytes]", &text[..end]),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::serve::adapters::ToolAdapter;
    use rig::tool::{Tool, ToolContext};

    #[tokio::test]
    async fn get_current_time_returns_a_parseable_utc_instant() {
        let output = ToolAdapter::new(GetCurrentTime)
            .run(serde_json::json!({}))
            .await
            .expect("the time tool should succeed");

        let json = output.as_json().expect("output should be structured JSON");
        let time: CurrentTime =
            serde_json::from_value(json.clone()).expect("output should match CurrentTime");

        let parsed =
            chrono::DateTime::parse_from_rfc3339(&time.utc).expect("utc field should be RFC 3339");
        assert_eq!(parsed.timestamp(), time.unix_seconds);
        assert!(time.unix_seconds > 1_700_000_000, "timestamp looks stale");
    }

    /// Runs the tool against a temporary directory, bypassing the agent.
    async fn run(command: &str, timeout_seconds: Option<u64>) -> ShellOutput {
        let dir = std::env::temp_dir();
        let tool = RunShell::new(dir);
        let mut context = ToolContext::default();
        Tool::call(
            &tool,
            &mut context,
            RunShellArgs {
                command: command.to_string(),
                timeout_seconds,
            },
        )
        .await
        .expect("the command should start")
    }

    #[tokio::test]
    async fn run_shell_reports_stdout_and_a_zero_exit() {
        let output = run("printf hello", None).await;
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, "hello");
        assert!(!output.truncated);
        assert!(!output.timed_out);
    }

    #[tokio::test]
    async fn run_shell_reports_a_failing_exit_code_rather_than_an_error() {
        let output = run("echo oops >&2; exit 3", None).await;
        assert_eq!(output.exit_code, Some(3));
        assert_eq!(output.stderr.trim(), "oops");
    }

    #[tokio::test]
    async fn run_shell_runs_in_the_configured_directory() {
        let expected = std::env::temp_dir()
            .canonicalize()
            .expect("the temp dir should exist");
        let output = run("pwd", None).await;
        let actual = std::path::PathBuf::from(output.stdout.trim())
            .canonicalize()
            .expect("pwd should name a real directory");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn run_shell_kills_a_command_that_outlives_its_timeout() {
        let output = run("sleep 30", Some(1)).await;
        assert!(output.timed_out, "the command should have been killed");
        assert_eq!(output.exit_code, None);
        assert!(output.stderr.contains("killed after 1s"));
    }

    #[tokio::test]
    async fn run_shell_gives_a_command_no_stdin_to_wait_on() {
        // With stdin inherited this would hang the whole turn; with it closed
        // `cat` sees EOF at once.
        let output = run("cat", Some(10)).await;
        assert!(!output.timed_out);
        assert_eq!(output.stdout, "");
    }

    #[tokio::test]
    async fn run_shell_caps_a_flood_of_output() {
        let output = run("yes rigagent | head -c 200000", Some(30)).await;
        assert!(output.truncated, "a 200 KB flood should be cut");
        assert!(
            output.stdout.len() < MAX_STREAM_BYTES + 64,
            "the clipped text should stay near the cap, got {}",
            output.stdout.len()
        );
        assert!(output.stdout.contains("more bytes]"));
    }

    #[tokio::test]
    async fn a_subagent_without_its_agent_reports_rather_than_panics() {
        // The cell is filled right after `AgentBuilder::build`. If that wiring
        // is ever broken, the tool must say so instead of unwrapping.
        let (events, _rx) = tokio::sync::mpsc::channel(4);
        let tool = SpawnAgent::new(
            std::sync::Arc::new(std::sync::OnceLock::new()),
            "preamble".to_string(),
            events,
        );
        let mut context = ToolContext::default();
        let error = Tool::call(
            &tool,
            &mut context,
            SpawnAgentArgs {
                task: "anything".to_string(),
                instructions: None,
            },
        )
        .await
        .expect_err("an unfilled cell cannot run a subagent");
        assert!(matches!(error, SpawnAgentError::NotReady));
    }

    #[test]
    fn clip_leaves_short_output_untouched() {
        let (text, cut) = clip(b"short");
        assert_eq!(text, "short");
        assert!(!cut);
    }

    #[test]
    fn clip_cuts_on_a_char_boundary() {
        // A multi-byte char straddling the cap must not be split in half.
        let source = "\u{00e7}".repeat(MAX_STREAM_BYTES);
        let (text, cut) = clip(source.as_bytes());
        assert!(cut);
        let kept = text.split('\n').next().expect("a first line");
        assert!(kept.chars().all(|c| c == '\u{00e7}'));
        assert!(kept.len() <= MAX_STREAM_BYTES);
    }
}

/// Turn cap for one subagent run.
///
/// Lower than the parent's: a subagent is a bounded errand, and the caps
/// multiply once an agent can start agents.
const SUBAGENT_MAX_TURNS: usize = 60;

/// Appended to the parent's preamble for a subagent run.
///
/// The subagent keeps the parent's tools and skills but reports once, in
/// prose, because only its final message survives: the whole point is that
/// the work it waded through stays out of the parent's conversation.
const SUBAGENT_RULES: &str = "\n\nYou are a subagent. You were given one errand by another agent and \
your reply is the only thing that returns to it: nothing you read, ran or \
called is visible to the caller. Finish the errand yourself with the tools \
you have, then answer with the findings and the facts the caller needs, \
including anything that failed or that you could not do. Do not ask \
questions, do not report progress, and do not start further subagents.";

/// Arguments to [`SpawnAgent`].
#[derive(Debug, Deserialize)]
pub struct SpawnAgentArgs {
    /// The errand, stated completely: the subagent sees none of this
    /// conversation.
    pub task: String,
    /// Extra instructions for this run, such as what to report.
    #[serde(default)]
    pub instructions: Option<String>,
}

/// What [`SpawnAgent`] hands back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAgentOutput {
    /// The subagent's final message.
    pub report: String,
    /// Tools it called, in order, so the caller can judge what the report
    /// rests on.
    pub tools_used: Vec<String>,
}

/// Runs a task in a fresh conversation and returns only the final answer.
///
/// The subagent is this agent cloned: same model, same tools, same skills,
/// but an empty history and its own preamble. That isolation is the feature —
/// a job that reads twenty files reports a paragraph instead of leaving twenty
/// files in the caller's context.
#[derive(Clone)]
pub struct SpawnAgent {
    /// The parent agent, filled in after it is built.
    ///
    /// The tool belongs to the agent that owns it, so it cannot be handed the
    /// agent at construction; `OnceLock` closes that circle exactly once.
    agent: std::sync::Arc<std::sync::OnceLock<rig::Agent>>,
    /// The parent's preamble, so a subagent inherits its skills index.
    preamble: String,
    /// Where nested activity is announced, so the interface is not blank
    /// while a subagent works.
    events: tokio::sync::mpsc::Sender<crate::agent::AgentEvent>,
}

impl SpawnAgent {
    pub fn new(
        agent: std::sync::Arc<std::sync::OnceLock<rig::Agent>>,
        preamble: String,
        events: tokio::sync::mpsc::Sender<crate::agent::AgentEvent>,
    ) -> Self {
        Self {
            agent,
            preamble,
            events,
        }
    }
}

/// Why a subagent run produced no report.
#[derive(Debug)]
pub enum SpawnAgentError {
    /// The agent was never published into the cell. A wiring mistake, not a
    /// runtime condition.
    NotReady,
    /// The subagent's own run failed.
    Run(String),
}

impl std::fmt::Display for SpawnAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReady => write!(f, "the agent is not available to clone"),
            Self::Run(message) => write!(f, "the subagent failed: {message}"),
        }
    }
}

impl std::error::Error for SpawnAgentError {}

impl rig::tool::Tool for SpawnAgent {
    const NAME: &'static str = "spawn_agent";

    type Args = SpawnAgentArgs;
    type Output = SpawnAgentOutput;
    type Error = SpawnAgentError;

    fn description(&self) -> String {
        format!(
            "Hand one self-contained task to a subagent and get back only its \
             final report. The subagent has the same model, tools and skills \
             as you, but an empty conversation: it cannot see this one, so \
             state the task completely, including any path, name or decision \
             it needs. Use it when the work would otherwise fill this \
             conversation with material you do not need to keep — searching a \
             codebase, reading many files, a long tool sequence whose \
             conclusion is what matters. Do not use it for work you can \
             finish in a couple of calls, for anything needing the user's \
             input, or to split a job whose parts depend on each other. It \
             runs for at most {SUBAGENT_MAX_TURNS} turns."
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description":
                        "The errand, stated completely. The subagent sees none of this conversation.",
                },
                "instructions": {
                    "type": "string",
                    "description": "Extra instructions for this run, such as what to report.",
                },
            },
            "required": ["task"],
        })
    }

    fn annotations(&self) -> rig::completion::ToolAnnotations {
        rig::completion::ToolAnnotations {
            title: Some("Run a subagent".to_string()),
            // A subagent has the parent's tools, so it can do whatever the
            // parent can, shell included.
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        }
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        use futures::StreamExt;
        use rig::agent::MultiTurnStreamItem;

        let agent = self.agent.get().ok_or(SpawnAgentError::NotReady)?;
        tracing::info!(task = %args.task, "starting a subagent");

        let mut preamble = format!("{}{SUBAGENT_RULES}", self.preamble);
        if let Some(extra) = args.instructions.as_deref().map(str::trim)
            && !extra.is_empty()
        {
            preamble.push_str("\n\n");
            preamble.push_str(extra);
        }

        // No `.history()`: an empty conversation is the whole point.
        let mut stream = agent
            .prompt(args.task.clone())
            .preamble(preamble)
            .max_turns(SUBAGENT_MAX_TURNS)
            .stream();

        let mut tools_used = Vec::new();
        let mut text = String::new();

        while let Some(item) = stream.next().await {
            let item = item.map_err(|err| SpawnAgentError::Run(err.to_string()))?;
            match item {
                MultiTurnStreamItem::StreamAssistantItem(rig::streaming::Item::Event(
                    rig::streaming::StreamEvent::Text { text: delta, .. },
                )) => text.push_str(&delta),
                MultiTurnStreamItem::ToolCall { tool_call } => {
                    let name = tool_call.function.name.to_string();
                    // Indented, so a nested call reads as one: the interface
                    // would otherwise sit silent for the whole errand.
                    let _ = self
                        .events
                        .send(crate::agent::AgentEvent::Notice(format!("    ↳ {name}")))
                        .await;
                    tools_used.push(name);
                }
                MultiTurnStreamItem::ModelTurnRetried { .. } => {
                    // The deltas so far were provisional. Keeping them would
                    // splice an abandoned answer onto the real one.
                    text.clear();
                }
                MultiTurnStreamItem::FinalResponse(response) => {
                    let output = response.output();
                    let report = if output.is_empty() { text } else { output };
                    tracing::info!(tools = tools_used.len(), "subagent finished");
                    return Ok(SpawnAgentOutput { report, tools_used });
                }
                _ => {}
            }
        }

        Err(SpawnAgentError::Run(
            "the subagent's stream ended without a response".into(),
        ))
    }
}
