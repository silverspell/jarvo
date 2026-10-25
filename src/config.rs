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

/// The default office address: localhost on a free port, reachable only here.
pub const DEFAULT_OFFICE_BIND: &str = "127.0.0.1:0";

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
    #[arg(long, env = "RIGAGENT_SKILLS_DIR", value_parser = any_path)]
    pub skills_dir: Option<PathBuf>,

    /// Working directory for the run_shell tool.
    ///
    /// Shell access is off unless this names an existing directory. Commands
    /// then run without confirmation, with this process's permissions; the
    /// directory is where they start, not a sandbox that contains them.
    #[arg(long, env = "RIGAGENT_SHELL_DIR", value_parser = any_path)]
    pub shell_dir: Option<PathBuf>,

    /// Directory holding the office registration; unset means no office.
    #[arg(long, env = "RIGAGENT_OFFICE_DIR", value_parser = any_path)]
    pub office_dir: Option<PathBuf>,

    /// This agent's name in the office; defaults to the directory's name.
    #[arg(long, env = "RIGAGENT_OFFICE_NAME")]
    pub office_name: Option<String>,

    /// Address the office server listens on.
    #[arg(
        long,
        env = "RIGAGENT_OFFICE_BIND",
        default_value = DEFAULT_OFFICE_BIND
    )]
    pub office_bind: String,

    /// Be the office server for this directory instead of a terminal agent.
    #[arg(long, hide = true, value_parser = any_path)]
    pub serve_office: Option<PathBuf>,

    /// Path to an optional MCP server configuration file.
    #[arg(long, env = "RIGAGENT_MCP_CONFIG", value_parser = any_path)]
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
    /// Directory holding the office registration. `None` means the office is off.
    // The office fields are read by the server and agent tasks in later tasks;
    // drop these allows once they are wired up.
    #[allow(dead_code)]
    pub office_dir: Option<PathBuf>,
    /// This agent's name in the office. `None` means name it after the directory.
    #[allow(dead_code)]
    pub office_name: Option<String>,
    /// Address the office server listens on.
    #[allow(dead_code)]
    pub office_bind: String,
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
            mcp_config: non_empty_path(cli.mcp_config),
            skills_dir: non_empty_path(cli.skills_dir),
            shell_dir: non_empty_path(cli.shell_dir),
            office_dir: non_empty_path(cli.office_dir),
            office_name: cli.office_name.filter(|name| !name.trim().is_empty()),
            office_bind: office_bind_or_default(cli.office_bind),
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

/// Accept any string as a path, empty included.
///
/// Clap's own `PathBuf` parser rejects an empty value, and an environment
/// variable that is present but blank — `RIGAGENT_SHELL_DIR=` on a line of a
/// `.env` — is exactly that, so the program refuses to start instead of
/// reading the variable as "off". Blankness is decided in [`non_empty_path`],
/// after parsing.
fn any_path(raw: &str) -> Result<PathBuf, std::convert::Infallible> {
    Ok(PathBuf::from(raw))
}

/// Treat a blank path as absent.
///
/// Clap hands back `Some("")` for an environment variable that is present but
/// empty, which `RIGAGENT_SHELL_DIR=` in a `.env` produces. That means "off",
/// not a directory named "", exactly as a blank `--model` means "default".
pub(crate) fn non_empty_path(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| !path.as_os_str().is_empty())
}

/// A blank bind address means the default, not an empty address.
///
/// `RIGAGENT_OFFICE_BIND=` in a `.env` arrives as an empty string, which is
/// not a `SocketAddr`; the server should pick localhost on a free port then.
pub(crate) fn office_bind_or_default(raw: String) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        DEFAULT_OFFICE_BIND.to_string()
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_blank_shell_directory_means_off() {
        let cli =
            Cli::try_parse_from(["rigagent", "--shell-dir", ""]).expect("arguments should parse");
        assert_eq!(
            cli.shell_dir.as_deref(),
            Some(Path::new("")),
            "clap keeps the empty value"
        );
        assert_eq!(non_empty_path(cli.shell_dir), None);

        let cli = Cli::try_parse_from(["rigagent", "--shell-dir", "/tmp"])
            .expect("arguments should parse");
        assert_eq!(
            non_empty_path(cli.shell_dir).as_deref(),
            Some(Path::new("/tmp"))
        );
    }

    #[test]
    fn a_blank_office_dir_means_off_rather_than_a_startup_failure() {
        let cli = Cli::try_parse_from(["rigagent", "--office-dir", ""])
            .expect("an empty path must parse, as it does for --shell-dir");
        assert_eq!(non_empty_path(cli.office_dir), None);

        let cli = Cli::try_parse_from(["rigagent", "--office-dir", "/tmp/office"])
            .expect("arguments should parse");
        assert_eq!(
            non_empty_path(cli.office_dir).as_deref(),
            Some(Path::new("/tmp/office"))
        );
    }

    #[test]
    fn the_office_binds_to_localhost_on_a_free_port_unless_told_otherwise() {
        let cli = Cli::try_parse_from(["rigagent"]).expect("parse");
        assert_eq!(cli.office_bind, "127.0.0.1:0");
    }

    #[test]
    fn a_blank_office_bind_means_localhost_on_a_free_port() {
        assert_eq!(office_bind_or_default(String::new()), "127.0.0.1:0");
        assert_eq!(office_bind_or_default("   ".to_string()), "127.0.0.1:0");
        assert_eq!(
            office_bind_or_default("0.0.0.0:8080".to_string()),
            "0.0.0.0:8080"
        );
    }

    #[test]
    fn serving_the_office_is_a_separate_mode() {
        let cli =
            Cli::try_parse_from(["rigagent", "--serve-office", "/tmp/office"]).expect("parse");
        assert_eq!(cli.serve_office.as_deref(), Some(Path::new("/tmp/office")));
    }

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
