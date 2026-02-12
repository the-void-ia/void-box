# void-box onboarding: agents in the void
# Run with: vhs docs/vhs/onboard.tape
# Requires: vhs (go install github.com/charmbracelet/vhs@latest), cargo

Output docs/vhs/onboard.gif
SetTheme "Catppuccin Mocha"
SetFontSize 14
SetWidth 1000
SetHeight 560
SetPadding 20

# Title
Type "echo '=== void-box: Claude-in-void workflow demo ==='"
Enter
Sleep 1s

# Run the canonical workflow example (mock sandbox)
Type "cargo run --example claude_workflow"
Enter
Sleep 3s

# Show integration test for the same workflow
Type "cargo test --test integration test_claude_workflow_plan_apply --no-fail-fast"
Enter
Sleep 2s

# Done
Type "echo 'Done. See docs/workflows.md and examples/claude_workflow.rs'"
Enter
Sleep 2s
