//! The one `/` command parser, shared by the terminal and the office server.
//!
//! Pure by design: `parse` only decides what one line asks for. The caller
//! keeps its transcript lines, its status and its quitting behaviour.

/// The commands that close the interface.
pub const QUIT_COMMANDS: [&str; 3] = ["/quit", "/exit", "/bye"];

/// What one line of input asks for.
#[derive(Debug, PartialEq)]
pub enum Input {
    Prompt(String),
    /// `/json <prompt>`
    Structured(String),
    /// `/<skill> <request>`
    Skill {
        name: String,
        request: String,
    },
    /// A bare `/json`.
    JsonWithoutAPrompt,
    /// `/clear`
    Clear,
    /// `/exit`, `/quit`, `/bye`.
    Quit,
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

/// Decide what one line of input asks for.
pub fn parse(text: &str, skills: &[String]) -> Input {
    let text = text.trim();

    if QUIT_COMMANDS.contains(&text) {
        return Input::Quit;
    }
    if let Some(rest) = text.strip_prefix("/json") {
        if rest.is_empty() {
            return Input::JsonWithoutAPrompt;
        }
        // `/jsonfoo` is not `/json`: the command has to be its own word.
        if rest.starts_with(char::is_whitespace) {
            return Input::Structured(rest.trim().to_string());
        }
    }
    if let Some(name) = skill_command(text, skills) {
        let request = text[1 + name.len()..].trim().to_string();
        return Input::Skill { name, request };
    }
    if text == "/clear" {
        return Input::Clear;
    }
    Input::Prompt(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parser_tells_the_shapes_apart() {
        let skills = vec!["teach".to_string()];
        assert!(matches!(parse("hello", &skills), Input::Prompt(_)));
        assert!(matches!(parse("/json do it", &skills), Input::Structured(p) if p == "do it"));
        assert!(matches!(parse("/json", &skills), Input::JsonWithoutAPrompt));
        assert!(
            matches!(parse("/jsonfoo", &skills), Input::Prompt(p) if p == "/jsonfoo"),
            "only an exact or whitespace-separated /json is a command"
        );
        assert!(
            matches!(parse("/teach rust", &skills), Input::Skill { name, .. } if name == "teach")
        );
        assert!(matches!(parse("/clear", &skills), Input::Clear));
        assert!(matches!(parse("/bye", &skills), Input::Quit));
        assert!(
            matches!(parse("/nope", &skills), Input::Prompt(_)),
            "an unknown slash command is ordinary text, as it is today"
        );
    }
}
