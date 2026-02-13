//! Skill: A declared capability that gets installed into a Box.
//!
//! Skills are the building blocks of the "Skill + Environment = Box" abstraction.
//! They represent what a Box *can do* -- procedural knowledge, structured tools,
//! CLI binaries, or the LLM agent itself.
//!
//! # Skill Types
//!
//! - **`Skill::mcp(name)`** -- MCP server providing structured tools
//! - **`Skill::cli(name)`** -- CLI binary for raw execution
//! - **`Skill::agent(name)`** -- LLM agent (the reasoning engine)
//! - **`Skill::remote(id)`** -- Procedural knowledge from [skills.sh](https://skills.sh)
//! - **`Skill::file(path)`** -- Local SKILL.md with procedural knowledge
//!
//! # Example
//!
//! ```no_run
//! use void_box::skill::Skill;
//!
//! let market_data = Skill::mcp("market-data-mcp")
//!     .description("Provides OHLCV and news data for equities");
//!
//! let reasoning = Skill::agent("claude-code")
//!     .description("Autonomous reasoning and code execution");
//!
//! let best_practices = Skill::remote("obra/superpowers/brainstorming")
//!     .description("Structured brainstorming methodology");
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

/// A declared capability that gets installed into a Box.
#[derive(Debug, Clone)]
pub struct Skill {
    /// The kind of skill
    pub kind: SkillKind,
    /// Human-readable name
    pub name: String,
    /// Optional description
    pub description_text: Option<String>,
}

/// The type of skill and its configuration.
#[derive(Debug, Clone)]
pub enum SkillKind {
    /// MCP server -- structured tools via Model Context Protocol (stdio transport).
    /// The binary must be available in the guest filesystem.
    Mcp {
        /// Command to run the MCP server
        command: String,
        /// Arguments to pass
        args: Vec<String>,
        /// Environment variables for the MCP server
        env: HashMap<String, String>,
    },
    /// CLI binary -- raw execution capability.
    /// The binary must be available in the guest filesystem.
    Cli {
        /// Path or name of the binary
        command: String,
    },
    /// LLM Agent -- the reasoning engine itself (e.g. "claude-code").
    Agent {
        /// Agent binary name
        command: String,
    },
    /// Remote skill from skills.sh -- procedural knowledge fetched from GitHub.
    /// Format: "owner/repo/skill-name" or "owner/repo" (uses repo name as skill).
    Remote {
        /// skills.sh identifier (e.g. "obra/superpowers/brainstorming")
        id: String,
    },
    /// Local SKILL.md file -- procedural knowledge from a local file.
    File {
        /// Path to the SKILL.md file
        path: PathBuf,
    },
}

impl Skill {
    // -- Constructors --

    /// Create an MCP server skill.
    ///
    /// The `name` is both the skill name and the default command.
    /// Use `.command()` to override if they differ.
    pub fn mcp(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            kind: SkillKind::Mcp {
                command: name.clone(),
                args: Vec::new(),
                env: HashMap::new(),
            },
            name,
            description_text: None,
        }
    }

    /// Create a CLI binary skill.
    pub fn cli(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            kind: SkillKind::Cli {
                command: name.clone(),
            },
            name,
            description_text: None,
        }
    }

    /// Create an LLM agent skill.
    pub fn agent(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            kind: SkillKind::Agent {
                command: name.clone(),
            },
            name,
            description_text: None,
        }
    }

    /// Create a remote skill from skills.sh.
    ///
    /// The `id` should be in the format "owner/repo/skill-name".
    pub fn remote(id: impl Into<String>) -> Self {
        let id = id.into();
        let name = id
            .rsplit('/')
            .next()
            .unwrap_or(&id)
            .to_string();
        Self {
            kind: SkillKind::Remote { id },
            name,
            description_text: None,
        }
    }

    /// Create a skill from a local SKILL.md file.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string();
        Self {
            kind: SkillKind::File { path },
            name,
            description_text: None,
        }
    }

    // -- Builder methods --

    /// Set a human-readable description for this skill.
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description_text = Some(desc.into());
        self
    }

    /// Override the command for MCP or CLI skills.
    pub fn command(mut self, cmd: impl Into<String>) -> Self {
        let cmd = cmd.into();
        match &mut self.kind {
            SkillKind::Mcp { command, .. } => *command = cmd,
            SkillKind::Cli { command } => *command = cmd,
            SkillKind::Agent { command } => *command = cmd,
            _ => {} // no-op for Remote/File
        }
        self
    }

    /// Add arguments (MCP skills only).
    pub fn args(mut self, args: &[&str]) -> Self {
        if let SkillKind::Mcp {
            args: ref mut a, ..
        } = self.kind
        {
            *a = args.iter().map(|s| s.to_string()).collect();
        }
        self
    }

    /// Add an environment variable (MCP skills only).
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let SkillKind::Mcp {
            env: ref mut e, ..
        } = self.kind
        {
            e.insert(key.into(), value.into());
        }
        self
    }

    /// Generate the MCP config JSON entry for this skill (if it's an MCP skill).
    ///
    /// Returns `None` for non-MCP skills.
    pub fn mcp_config_entry(&self) -> Option<serde_json::Value> {
        match &self.kind {
            SkillKind::Mcp { command, args, env } => {
                let mut entry = serde_json::json!({
                    "command": command,
                    "args": args,
                });
                if !env.is_empty() {
                    entry["env"] = serde_json::json!(env);
                }
                Some(entry)
            }
            _ => None,
        }
    }

    /// Fetch the SKILL.md content from skills.sh (GitHub raw content).
    ///
    /// For `Skill::remote("owner/repo/skill-name")`, fetches from:
    /// `https://raw.githubusercontent.com/{owner}/{repo}/main/skills/{skill-name}/SKILL.md`
    ///
    /// Returns the full SKILL.md content as a String.
    /// Returns an error if the skill is not a Remote kind, the URL can't be built,
    /// or the HTTP request fails.
    pub async fn fetch_remote_content(&self) -> crate::Result<String> {
        let url = self.remote_url().ok_or_else(|| {
            crate::Error::Config(format!("Skill '{}' has no remote URL", self.name))
        })?;

        eprintln!("  [skill] Fetching {} ...", url);

        let resp = reqwest::get(&url).await.map_err(|e| {
            crate::Error::Config(format!("Failed to fetch skill from {}: {}", url, e))
        })?;

        if !resp.status().is_success() {
            return Err(crate::Error::Config(format!(
                "Failed to fetch skill from {} (HTTP {})",
                url,
                resp.status()
            )));
        }

        resp.text().await.map_err(|e| {
            crate::Error::Config(format!("Failed to read skill body from {}: {}", url, e))
        })
    }

    /// Build the raw GitHub URL for a remote skill.
    ///
    /// For `Skill::remote("owner/repo/skill-name")`, returns:
    /// `https://raw.githubusercontent.com/{owner}/{repo}/main/skills/{skill-name}/SKILL.md`
    ///
    /// Returns `None` if this is not a Remote skill or the id has fewer than 2 parts.
    pub fn remote_url(&self) -> Option<String> {
        match &self.kind {
            SkillKind::Remote { id } => {
                let parts: Vec<&str> = id.splitn(3, '/').collect();
                if parts.len() == 3 {
                    Some(format!(
                        "https://raw.githubusercontent.com/{}/{}/main/skills/{}/SKILL.md",
                        parts[0], parts[1], parts[2]
                    ))
                } else if parts.len() == 2 {
                    Some(format!(
                        "https://raw.githubusercontent.com/{}/{}/main/SKILL.md",
                        parts[0], parts[1]
                    ))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_mcp() {
        let s = Skill::mcp("market-data-mcp")
            .description("Market data provider")
            .args(&["--mode", "mock"]);
        assert_eq!(s.name, "market-data-mcp");
        assert!(s.description_text.is_some());
        assert!(s.mcp_config_entry().is_some());
    }

    #[test]
    fn test_skill_agent() {
        let s = Skill::agent("claude-code")
            .description("Autonomous reasoning");
        assert_eq!(s.name, "claude-code");
        assert!(matches!(s.kind, SkillKind::Agent { .. }));
    }

    #[test]
    fn test_skill_remote() {
        let s = Skill::remote("obra/superpowers/brainstorming");
        assert_eq!(s.name, "brainstorming");
        assert_eq!(
            s.remote_url().unwrap(),
            "https://raw.githubusercontent.com/obra/superpowers/main/skills/brainstorming/SKILL.md"
        );
    }

    #[test]
    fn test_skill_file() {
        let s = Skill::file("skills/financial-data-analysis.md");
        assert_eq!(s.name, "financial-data-analysis");
    }

    #[test]
    fn test_skill_cli() {
        let s = Skill::cli("quant-tools")
            .description("Technical indicator calculator");
        assert_eq!(s.name, "quant-tools");
        assert!(matches!(s.kind, SkillKind::Cli { .. }));
    }

    #[test]
    fn test_skill_remote_url_two_part() {
        let s = Skill::remote("owner/repo");
        assert_eq!(s.name, "repo");
        assert_eq!(
            s.remote_url().unwrap(),
            "https://raw.githubusercontent.com/owner/repo/main/SKILL.md"
        );
    }

    #[test]
    fn test_skill_remote_url_invalid() {
        let s = Skill::remote("justname");
        assert!(s.remote_url().is_none());
    }

    #[tokio::test]
    #[ignore] // Requires network access -- run with: cargo test -- --ignored test_fetch_remote_skill_live
    async fn test_fetch_remote_skill_live() {
        let s = Skill::remote("vercel-labs/skills/find-skills");
        let content = s.fetch_remote_content().await.unwrap();
        assert!(content.contains("# Find Skills"), "Expected '# Find Skills' in fetched SKILL.md");
        assert!(content.len() > 100, "Content should be substantial");
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_fetch_remote_skill_not_found() {
        let s = Skill::remote("nonexistent-org/nonexistent-repo/nonexistent-skill");
        let result = s.fetch_remote_content().await;
        assert!(result.is_err(), "Should fail for nonexistent skill");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("404"), "Error should mention 404: {}", err_msg);
    }
}
