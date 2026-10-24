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

#[cfg(test)]
mod tests {
    use super::*;
    use rig::serve::adapters::ToolAdapter;

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
}
