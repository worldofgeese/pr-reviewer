use std::path::Path;

use tokio::process::Command;

use crate::config::ReasoningEffort;
use crate::harness::Harness;

pub struct ClaudeCodeHarness;

impl Harness for ClaudeCodeHarness {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn executable(&self) -> &'static str {
        "claude"
    }

    fn build_command(
        &self,
        _prompt: &str,
        model: &str,
        _reasoning_effort: Option<ReasoningEffort>,
        _working_dir: &Path,
        tools: &[String],
    ) -> Command {
        let mut cmd = Command::new("claude");
        // `--tools` removes every built-in tool that is not listed, so the
        // session cannot call it. An empty list (`--tools ""`) removes all.
        cmd.arg("--model")
            .arg(model)
            .arg("--tools")
            .arg(tools.join(","))
            // `--tools` covers built-in tools only; skip the user's MCP servers too.
            .arg("--strict-mcp-config")
            .arg("--dangerously-skip-permissions")
            .arg("-p")
            .arg("-");
        cmd
    }

    fn uses_stdin(&self) -> bool {
        true
    }

    fn limits_tools(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tools: &[&str]) -> Vec<String> {
        let tools: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
        let cmd = ClaudeCodeHarness.build_command("p", "m", None, Path::new("."), &tools);
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn tools_arg(args: &[String]) -> &str {
        let at = args.iter().position(|a| a == "--tools").expect("--tools");
        &args[at + 1]
    }

    #[test]
    fn a_session_is_limited_to_the_tools_it_is_given() {
        let args = args(&["Read", "Grep"]);
        assert_eq!(tools_arg(&args), "Read,Grep");
    }

    #[test]
    fn an_empty_tool_set_disables_every_tool() {
        let args = args(&[]);
        assert_eq!(tools_arg(&args), "");
    }

    #[test]
    fn the_tool_set_is_one_argument_before_the_prompt_flags() {
        let args = args(&["Read"]);
        assert_eq!(
            args,
            [
                "--model",
                "m",
                "--tools",
                "Read",
                "--strict-mcp-config",
                "--dangerously-skip-permissions",
                "-p",
                "-"
            ]
        );
    }
}
