//! CLI arguments and environment configuration.
//!
//! Provider selection is entirely data-driven: a `--provider`/`--model` pair on
//! the command line, or the `RIGAGENT_PROVIDER`/`RIGAGENT_MODEL` environment
//! variables. No Rust source needs editing to switch providers.
//!
//! API keys are read here and never logged.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};

/// The default Ollama endpoint, used when `OLLAMA_BASE_URL` is unset.
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// A supported model provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Provider {
    /// OpenAI, authenticated with `OPENAI_API_KEY`.
    Openai,
    /// DeepSeek, authenticated with `DEEPSEEK_API_KEY`.
    Deepseek,
    /// A local or remote Ollama daemon at `OLLAMA_BASE_URL`.
    Ollama,
}

impl Provider {
    /// The provider's display name, shown in the TUI header.
    pub fn label(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Deepseek => "deepseek",
            Self::Ollama => "ollama",
        }
    }

    /// The model used when none is given on the command line.
    fn default_model(self) -> &'static str {
        match self {
            Self::Openai => "gpt-4o-mini",
            Self::Deepseek => "deepseek-chat",
            Self::Ollama => "llama3.2",
        }
    }

    /// A conservative context window for the provider, used only when the
    /// model is not in [`KNOWN_CONTEXT_WINDOWS`].
    ///
    /// Windows vary by model, not by provider, so this is a floor rather than
    /// a guess at the truth: better to show a fuller gauge than to imply room
    /// that is not there.
    fn fallback_context_window(self) -> u64 {
        match self {
            Self::Openai => 128_000,
            Self::Deepseek => 64_000,
            // Ollama serves whatever `num_ctx` the model file sets, commonly
            // far below the architecture's maximum.
            Self::Ollama => 8_192,
        }
    }
}

/// Context windows, in tokens, for models this project has been run against.
///
/// Matched as prefixes so a dated variant resolves to its base model, longest
/// first because `gpt-4o` is a prefix of `gpt-4o-mini`. No provider reports
/// its own limit, so this table is maintained by hand and will fall behind;
/// `--context-window` overrides it for anything missing or wrong.
const KNOWN_CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("deepseek-reasoner", 64_000),
    ("deepseek-flash", 1_000_000),
    ("deepseek-chat", 64_000),
    ("gpt-4o-mini", 128_000),
    ("gpt-4o", 128_000),
];

/// The context window assumed for `model`, falling back to the provider's.
fn context_window_for(provider: Provider, model: &str) -> u64 {
    KNOWN_CONTEXT_WINDOWS
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map(|(_, window)| *window)
        .unwrap_or_else(|| provider.fallback_context_window())
}

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "rigagent",
    about = "A terminal AI agent built on Rig and Ratatui",
    version
)]
pub struct Cli {
    /// Model provider to talk to.
    #[arg(short, long, value_enum, env = "RIGAGENT_PROVIDER")]
    pub provider: Option<Provider>,

    /// Model name, as the provider spells it.
    #[arg(short, long, env = "RIGAGENT_MODEL")]
    pub model: Option<String>,

    /// System prompt handed to the agent.
    #[arg(long, env = "RIGAGENT_SYSTEM_PROMPT")]
    pub system_prompt: Option<String>,

    /// Maximum agent turns per message (model calls plus tool rounds).
    ///
    /// Generous by default: a browsing task driven through MCP tools can take
    /// dozens of rounds. It is a safety stop against a model looping forever,
    /// not a budget.
    #[arg(long, default_value_t = 500, env = "RIGAGENT_MAX_TURNS")]
    pub max_turns: usize,

    /// Context window assumed for the model, in tokens.
    ///
    /// Only drives the footer's usage gauge; it does not limit anything.
    #[arg(long, env = "RIGAGENT_CONTEXT_WINDOW")]
    pub context_window: Option<u64>,

    /// Directory holding a skill library (`*/SKILL.md`).
    ///
    /// Each skill's name and description go into the preamble; bodies are
    /// opened on demand with the read_skill tool.
    #[arg(long, env = "RIGAGENT_SKILLS_DIR")]
    pub skills_dir: Option<PathBuf>,

    /// Working directory for the run_shell tool.
    ///
    /// Shell access is off unless this is set: the tool is only registered
    /// when there is a directory to run commands in.
    #[arg(long, env = "RIGAGENT_SHELL_DIR")]
    pub shell_dir: Option<PathBuf>,

    /// Path to an optional MCP server configuration file.
    #[arg(long, env = "RIGAGENT_MCP_CONFIG")]
    pub mcp_config: Option<PathBuf>,
}

/// Fully resolved configuration: CLI arguments merged with the environment.
#[derive(Debug, Clone)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    pub system_prompt: String,
    pub max_turns: usize,
    /// Assumed context window, in tokens, for the footer's usage gauge.
    pub context_window: u64,
    pub mcp_config: Option<PathBuf>,
    /// Directory holding the skill library, if one is configured.
    pub skills_dir: Option<PathBuf>,
    /// Working directory for shell commands. `None` disables the tool.
    pub shell_dir: Option<PathBuf>,
    /// The provider credential, if the chosen provider needs one.
    pub api_key: Option<String>,
    /// Base URL of the Ollama daemon (only meaningful for [`Provider::Ollama`]).
    pub ollama_base_url: String,
}

/// The default system prompt. Mentions the time tool so a tool-capable model
/// reaches for it instead of guessing.
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant running inside a terminal UI. \
Keep answers concise and use plain text. \
When the user asks about the current date or time, call the get_current_time tool instead of guessing.";

impl Config {
    /// Resolve configuration from parsed arguments and the process environment.
    ///
    /// Call [`load_dotenv`] first so a `.env` file is visible here.
    pub fn resolve(cli: Cli) -> Result<Self> {
        let provider = cli.provider.unwrap_or(Provider::Openai);
        let model = cli
            .model
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| provider.default_model().to_string());

        let api_key = match provider {
            Provider::Openai => Some(require_key("OPENAI_API_KEY", provider)?),
            Provider::Deepseek => Some(require_key("DEEPSEEK_API_KEY", provider)?),
            // Ollama is normally unauthenticated; a token is honoured if present.
            Provider::Ollama => non_empty_env("OLLAMA_API_KEY"),
        };

        let ollama_base_url =
            non_empty_env("OLLAMA_BASE_URL").unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());

        let context_window = cli
            .context_window
            .filter(|window| *window > 0)
            .unwrap_or_else(|| context_window_for(provider, &model));

        Ok(Self {
            provider,
            model,
            system_prompt: cli
                .system_prompt
                .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string()),
            max_turns: cli.max_turns.max(1),
            context_window,
            mcp_config: cli.mcp_config,
            skills_dir: cli.skills_dir,
            shell_dir: cli.shell_dir,
            api_key,
            ollama_base_url,
        })
    }

    /// `provider:model`, for the TUI header.
    pub fn header_label(&self) -> String {
        format!("{}:{}", self.provider.label(), self.model)
    }
}

/// Load `.env` from the working directory, if one exists. A missing file is not
/// an error; anything else is reported.
pub fn load_dotenv() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(err) if err.not_found() => Ok(()),
        Err(err) => Err(err).context("failed to read .env"),
    }
}

/// An environment variable's value, treating blank as unset.
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A required credential. The error names the variable but never its value.
fn require_key(name: &str, provider: Provider) -> Result<String> {
    match non_empty_env(name) {
        Some(key) => Ok(key),
        None => bail!(
            "provider `{}` needs {name}; set it in the environment or in a .env file",
            provider.label()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_labels_match_cli_values() {
        assert_eq!(Provider::Openai.label(), "openai");
        assert_eq!(Provider::Deepseek.label(), "deepseek");
        assert_eq!(Provider::Ollama.label(), "ollama");
    }

    #[test]
    fn parses_provider_and_model_from_argv() {
        let cli = Cli::try_parse_from([
            "rigagent",
            "--provider",
            "deepseek",
            "--model",
            "deepseek-chat",
        ])
        .expect("arguments should parse");
        assert_eq!(cli.provider, Some(Provider::Deepseek));
        assert_eq!(cli.model.as_deref(), Some("deepseek-chat"));
    }

    #[test]
    fn each_provider_has_a_default_model() {
        for provider in [Provider::Openai, Provider::Deepseek, Provider::Ollama] {
            assert!(!provider.default_model().is_empty());
        }
    }

    #[test]
    fn rejects_unknown_provider() {
        assert!(Cli::try_parse_from(["rigagent", "--provider", "nope"]).is_err());
    }

    #[test]
    fn the_context_window_follows_the_model_not_the_provider() {
        // The headline case: deepseek-flash is a 1M-token model, while the
        // provider's other models are 64k. A provider-wide default got this
        // wrong by a factor of sixteen.
        assert_eq!(
            context_window_for(Provider::Deepseek, "deepseek-flash"),
            1_000_000
        );
        assert_eq!(
            context_window_for(Provider::Deepseek, "deepseek-chat"),
            64_000
        );

        // Longest prefix wins, so gpt-4o does not swallow gpt-4o-mini.
        assert_eq!(context_window_for(Provider::Openai, "gpt-4o-mini"), 128_000);
        assert_eq!(context_window_for(Provider::Openai, "gpt-4o"), 128_000);

        // A dated variant resolves to its base model.
        assert_eq!(
            context_window_for(Provider::Deepseek, "deepseek-flash-2026-10"),
            1_000_000
        );

        // Anything unknown falls back to the provider's floor.
        assert_eq!(context_window_for(Provider::Ollama, "llama3.2"), 8_192);
        assert_eq!(
            context_window_for(Provider::Deepseek, "deepseek-something-new"),
            64_000
        );
    }

    #[test]
    fn max_turns_defaults_high_and_is_overridable() {
        let cli = Cli::try_parse_from(["rigagent"]).expect("bare arguments should parse");
        assert_eq!(cli.max_turns, 500);

        let cli = Cli::try_parse_from(["rigagent", "--max-turns", "3"]).expect("should parse");
        assert_eq!(cli.max_turns, 3);
    }
}
