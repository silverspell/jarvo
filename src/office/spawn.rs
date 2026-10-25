//! Starting a Jarvo from the room, through tmux.

use std::io;
use std::path::Path;
use std::process::Command;

/// The tmux command line that starts a Jarvo in `cwd`, named `name`.
pub fn tmux_command(exe: &Path, dir: &Path, cwd: &Path, name: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        format!("jarvo-{}", session_name(name)),
        "-c".into(),
        cwd.to_string_lossy().into_owned(),
        exe.to_string_lossy().into_owned(),
        "--office-dir".into(),
        dir.to_string_lossy().into_owned(),
        "--office-name".into(),
        name.into(),
    ]
}

/// The tmux invocation for a spawned Jarvo, with the shell door closed.
fn spawn_command(exe: &Path, dir: &Path, cwd: &Path, name: &str) -> Command {
    let argv = tmux_command(exe, dir, cwd, name);
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    // dotenvy never overrides an already-set variable, so a blank value here
    // beats both an inherited setting and any `.env` in the room-chosen cwd.
    command.env("RIGAGENT_SHELL_DIR", "");
    command
}

/// Run tmux without a shell and report its stderr if it cannot start.
pub fn spawn(exe: &Path, dir: &Path, cwd: &Path, name: &str) -> io::Result<()> {
    require_directory(cwd)?;
    let output = spawn_command(exe, dir, cwd, name).output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    Err(io::Error::other(if message.is_empty() {
        format!("tmux exited with {}", output.status)
    } else {
        message.to_string()
    }))
}

/// The cwd must exist as a directory before tmux is asked to use it.
fn require_directory(cwd: &Path) -> io::Result<()> {
    if cwd.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("`{}` is not a directory", cwd.display()),
        ))
    }
}

/// A session name tmux cannot mistake for an option or a target.
fn session_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let safe = safe.trim_matches('-');
    if safe.is_empty() {
        "agent".into()
    } else {
        safe.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spawn_command_names_the_session_and_never_enables_the_shell() {
        let argv = tmux_command(
            Path::new("/usr/local/bin/rigagent"),
            Path::new("/home/me/.jarvo/office"),
            Path::new("/home/me/code/api"),
            "api",
        );
        assert_eq!(argv[0], "tmux");
        assert!(argv.windows(2).any(|w| w[0] == "-s" && w[1] == "jarvo-api"));
        assert!(argv.iter().any(|a| a == "--office-dir"));
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--office-name" && w[1] == "api"),
            "the name chosen in the room reaches the new Jarvo"
        );
        assert!(
            !argv.iter().any(|a| a == "--shell-dir"),
            "no shell from the room"
        );
    }

    #[test]
    fn an_unsafe_name_is_sanitized_for_tmux_but_kept_for_the_office() {
        let argv = tmux_command(
            Path::new("/usr/local/bin/rigagent"),
            Path::new("/home/me/.jarvo/office"),
            Path::new("/home/me/code/api"),
            "a b:c",
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-s" && w[1] == "jarvo-a-b-c"),
            "tmux only sees a safe session name: {argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--office-name" && w[1] == "a b:c"),
            "the display name reaches the office unchanged: {argv:?}"
        );
        assert_eq!(
            session_name("::"),
            "agent",
            "a name with no safe characters"
        );
    }

    #[test]
    fn the_spawned_agent_cannot_get_a_shell_from_the_environment() {
        let command = spawn_command(
            Path::new("/usr/local/bin/rigagent"),
            Path::new("/home/me/.jarvo/office"),
            Path::new("/home/me/code/api"),
            "api",
        );
        let (_, shell) = command
            .get_envs()
            .find(|(key, _)| key.to_string_lossy() == "RIGAGENT_SHELL_DIR")
            .expect("the child environment pins RIGAGENT_SHELL_DIR");
        assert!(
            shell.is_some_and(|value| value.is_empty()),
            "an empty RIGAGENT_SHELL_DIR turns the shell off: {shell:?}"
        );
    }

    #[test]
    fn a_missing_or_non_directory_cwd_is_refused() {
        assert!(
            require_directory(Path::new("/definitely/not/a/directory/here")).is_err(),
            "a missing cwd is refused"
        );
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(
            require_directory(&file).is_err(),
            "a regular file is not a cwd"
        );
        assert!(
            require_directory(Path::new(env!("CARGO_MANIFEST_DIR"))).is_ok(),
            "a real directory is accepted"
        );
    }
}
