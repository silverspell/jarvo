# Regenerate with:  vhs docs/demo.tape
# Needs a provider key in .env; the recording makes real model calls.

Output docs/demo.gif

Set Shell "zsh"
Set FontSize 15
Set Width 1180
Set Height 720
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 55ms

# Start the agent off camera: MCP servers take a few seconds to come up.
Hide
Type "./target/debug/rigagent"
Enter
Sleep 9s
Show

Sleep 1.5s
Type "what time is it?"
Sleep 600ms
Enter
Sleep 9s

Type "/json three reasons to write a TUI in Rust"
Sleep 600ms
Enter
Sleep 14s

# A subagent errand: the nested run_shell shows indented, and only the
# report comes back into the conversation.
Type "ask a subagent how many Rust files are in src/ and how many lines in total"
Sleep 600ms
Enter
Sleep 30s

Type "/exit"
Sleep 800ms
Enter
Sleep 2s
