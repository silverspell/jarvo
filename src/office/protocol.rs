use serde::{Deserialize, Serialize};

/// Stable for the life of one process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentId(pub String);

/// Monotonic per agent, starts at 1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentInfo {
    pub id: AgentId,
    /// Display name.
    pub name: String,
    /// For example `"deepseek:deepseek-chat"`.
    pub model: String,
    pub cwd: String,
    /// So the browser can list `/<skill>`.
    pub skills: Vec<String>,
}

/// Jarvo -> server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromAgent {
    Hello { info: AgentInfo },
    Turn { turn: TurnId, event: TurnEvent },
}

/// Server -> Jarvo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToAgent {
    Prompt { text: String },
    Structured { text: String },
    Skill { name: String, request: String },
}

/// Mirrors `AgentEvent`, plus queue news.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    Started { prompt: String },
    Delta { text: String },
    ToolCall { name: String },
    ToolResult { name: String },
    Notice { text: String },
    ContextTokens { tokens: u64 },
    Completed { text: String },
    Failed { message: String },
    Queued { text: String, depth: usize },
}

/// Server -> browser.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToBrowser {
    Roster {
        agents: Vec<AgentInfo>,
    },
    Joined {
        info: AgentInfo,
    },
    Left {
        id: AgentId,
    },
    Turn {
        id: AgentId,
        turn: TurnId,
        event: TurnEvent,
    },
    Tail {
        id: AgentId,
        events: Vec<(TurnId, TurnEvent)>,
    },
    Refused {
        id: AgentId,
        reason: String,
    },
}

/// Browser -> server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromBrowser {
    Select {
        id: AgentId,
    },
    /// Raw, exactly as typed.
    Send {
        id: AgentId,
        text: String,
    },
    Spawn {
        cwd: String,
        name: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_event_round_trips_through_json() {
        let event = FromAgent::Turn {
            turn: TurnId(1),
            event: TurnEvent::Delta { text: "hi".into() },
        };
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains(r#""type":"turn""#), "tagged: {json}");
        let back: FromAgent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, event);
    }

    #[test]
    fn an_unknown_message_type_is_an_error_not_a_panic() {
        let result: Result<ToAgent, _> = serde_json::from_str(r#"{"type":"nope"}"#);
        assert!(result.is_err());
    }
}
