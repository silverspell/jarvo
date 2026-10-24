//! Optional MCP client setup.
//!
//! MCP is configured by a JSON file naming stdio servers to launch:
//!
//! ```json
//! {
//!   "servers": [
//!     {
//!       "name": "filesystem",
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
//!       "env": { "LOG_LEVEL": "warn" }
//!     }
//!   ]
//! }
//! ```
//!
//! Nothing here is required to run the application: with no config file, an
//! unreadable one, or a server that fails to start, startup continues and the
//! problem is reported as a warning.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use rig::tool::DynamicTool;
use rig::tool::rmcp::McpConnection;
use rmcp::ServiceExt;
use rmcp::model::ClientConfig;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

/// The MCP configuration file.
#[derive(Debug, Default, Deserialize)]
pub struct McpConfigFile {
    /// Servers to launch, in order.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One stdio MCP server.
#[derive(Debug, Deserialize)]
pub struct McpServerConfig {
    /// A label used in logs and status messages.
    pub name: String,
    /// Executable to run.
    pub command: String,
    /// Arguments passed to the executable.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Set to `false` to keep a server in the file but not start it.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Tools gathered from MCP servers, plus the live connections behind them.
///
/// A connection closes when the last clone is dropped, so the caller must keep
/// [`connections`](Self::connections) alive for as long as the tools are used.
#[derive(Default)]
pub struct McpTools {
    /// Tools to register on the agent.
    pub tools: Vec<DynamicTool>,
    /// Connections keeping the servers running.
    pub connections: Vec<McpConnection>,
    /// Human-readable notes about what was (or was not) loaded.
    pub notes: Vec<String>,
}

impl McpTools {
    /// Whether any MCP tool was loaded.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Parse an MCP configuration file.
pub fn parse_config(contents: &str) -> Result<McpConfigFile> {
    serde_json::from_str(contents).context("MCP config is not valid JSON")
}

/// Launch every enabled server in the config at `path` and collect their tools.
///
/// `path` of `None`, or a path that does not exist, yields no tools and no
/// error: MCP is opt-in. A single failing server is reported in
/// [`McpTools::notes`] and does not stop the others.
pub async fn load(path: Option<&Path>) -> McpTools {
    let mut loaded = McpTools::default();

    let Some(path) = path else {
        return loaded;
    };
    if !path.exists() {
        loaded
            .notes
            .push(format!("MCP: no config at {}", path.display()));
        return loaded;
    }

    let config = match std::fs::read_to_string(path)
        .context("failed to read MCP config")
        .and_then(|contents| parse_config(&contents))
    {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(error = %err, path = %path.display(), "ignoring MCP config");
            loaded.notes.push(format!("MCP: {err}"));
            return loaded;
        }
    };

    for server in config.servers.iter().filter(|server| server.enabled) {
        match connect(server).await {
            Ok((connection, tools)) => {
                let names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
                tracing::info!(server = %server.name, tools = ?names, "MCP server connected");
                loaded.notes.push(format!(
                    "MCP: {} provided {} tool(s)",
                    server.name,
                    tools.len()
                ));
                loaded.tools.extend(tools);
                loaded.connections.push(connection);
            }
            Err(err) => {
                tracing::warn!(server = %server.name, error = %err, "MCP server unavailable");
                loaded
                    .notes
                    .push(format!("MCP: {} unavailable ({err})", server.name));
            }
        }
    }

    loaded
}

/// Start one server and convert its tool list into dynamic Rig tools.
async fn connect(server: &McpServerConfig) -> Result<(McpConnection, Vec<DynamicTool>)> {
    let command = tokio::process::Command::new(&server.command).configure(|command| {
        command.args(&server.args);
        for (key, value) in &server.env {
            command.env(key, value);
        }
    });

    // rmcp inherits the child's stderr by default, which would write straight
    // over the TUI. Capture it instead and forward it to the log.
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn `{}`", server.command))?;
    if let Some(stderr) = stderr {
        tokio::spawn(log_stderr(server.name.clone(), stderr));
    }

    let service = ClientConfig::default()
        .serve(transport)
        .await
        .context("MCP initialization failed")?;

    let connection = McpConnection::new(service);
    let mut tools = Vec::new();
    for tool in connection
        .tools()
        .await
        .context("failed to list MCP tools")?
    {
        let name = tool.name().to_string();
        match DynamicTool::try_from(tool) {
            Ok(tool) => tools.push(tool),
            Err(err) => tracing::warn!(tool = %name, error = %err, "skipping MCP tool"),
        }
    }

    Ok((connection, tools))
}

/// Forward an MCP server's stderr to the log, one line at a time, until the
/// process exits.
async fn log_stderr(server: String, stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if !line.trim().is_empty() {
                    tracing::debug!(server = %server, "{line}");
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(server = %server, error = %err, "stderr closed");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_server_list() {
        let config = parse_config(
            r#"{ "servers": [
                 { "name": "fs", "command": "npx", "args": ["-y", "server"],
                   "env": { "A": "b" } }
               ] }"#,
        )
        .expect("config should parse");

        assert_eq!(config.servers.len(), 1);
        let server = &config.servers[0];
        assert_eq!(server.name, "fs");
        assert_eq!(server.command, "npx");
        assert_eq!(server.args, ["-y", "server"]);
        assert_eq!(server.env.get("A").map(String::as_str), Some("b"));
        assert!(server.enabled, "servers are enabled unless disabled");
    }

    #[test]
    fn an_empty_object_means_no_servers() {
        assert!(
            parse_config("{}")
                .expect("empty config is valid")
                .servers
                .is_empty()
        );
    }

    #[tokio::test]
    async fn missing_config_is_not_an_error() {
        let loaded = load(None).await;
        assert!(loaded.is_empty());
        assert!(loaded.notes.is_empty());

        let loaded = load(Some(Path::new("/nonexistent/rigagent-mcp.json"))).await;
        assert!(loaded.is_empty());
        assert_eq!(
            loaded.notes.len(),
            1,
            "a missing path is reported, not fatal"
        );
    }

    #[tokio::test]
    async fn a_server_that_cannot_start_is_reported_not_fatal() {
        let dir = std::env::temp_dir().join("rigagent-mcp-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("mcp.json");
        std::fs::write(
            &path,
            r#"{ "servers": [{ "name": "broken", "command": "rigagent-no-such-binary" }] }"#,
        )
        .expect("write config");

        let loaded = load(Some(&path)).await;
        assert!(loaded.is_empty());
        assert!(
            loaded.notes.iter().any(|note| note.contains("broken")),
            "the failure should be reported: {:?}",
            loaded.notes
        );
    }
}
