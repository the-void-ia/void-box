//! Remote Skills Example: Fetch SKILL.md files from skills.sh
//!
//! Demonstrates fetching real skills from the open agent skills ecosystem
//! (https://skills.sh) and provisioning them into a void-box AgentBox.
//!
//! Each skill is fetched live from GitHub (raw.githubusercontent.com),
//! printed with a preview, and installed into a mock Box.
//!
//! ## Usage
//!
//!   cargo run --example remote_skills
//!
//! Requires network access to fetch from GitHub.

use std::error::Error;

use void_box::agent_box::AgentBox;
use void_box::skill::Skill;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║        Remote Skills: Fetching from skills.sh              ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // ---- Define remote skills from the skills.sh ecosystem ----

    let skills = vec![
        Skill::remote("obra/superpowers/brainstorming")
            .description("Structured brainstorming methodology (18.7K installs)"),
        Skill::remote("obra/superpowers/systematic-debugging")
            .description("Systematic debugging methodology (10.2K installs)"),
        Skill::remote("obra/superpowers/test-driven-development")
            .description("Test-driven development methodology (8.5K installs)"),
    ];

    // ---- Fetch each skill and show a preview ----

    println!("--- Fetching {} remote skills ---", skills.len());
    println!();

    let mut fetched_count = 0;
    let mut failed_count = 0;

    for skill in &skills {
        let desc = skill
            .description_text
            .as_deref()
            .unwrap_or("(no description)");

        println!("  Skill: {} -- {}", skill.name, desc);

        if let Some(url) = skill.remote_url() {
            println!("  URL:   {}", url);
        }

        match skill.fetch_remote_content().await {
            Ok(content) => {
                fetched_count += 1;
                let lines: Vec<&str> = content.lines().collect();
                let preview_lines = lines.len().min(8);

                println!("  Size:  {} bytes ({} lines)", content.len(), lines.len());
                println!("  Preview:");
                for line in &lines[..preview_lines] {
                    println!("    | {}", line);
                }
                if lines.len() > preview_lines {
                    println!("    | ... ({} more lines)", lines.len() - preview_lines);
                }
                println!();
            }
            Err(e) => {
                failed_count += 1;
                println!("  ERROR: {}", e);
                println!();
            }
        }
    }

    println!(
        "--- Fetched: {} OK, {} failed ---",
        fetched_count, failed_count
    );
    println!();

    // ---- Build a Box with these skills ----

    println!("--- Building AgentBox with remote skills ---");
    println!();

    let reasoning = Skill::agent("claude-code")
        .description("Autonomous reasoning and code execution");

    let mut builder = AgentBox::new("developer")
        .skill(reasoning)
        .prompt(
            "You are a senior developer. Use your brainstorming, debugging, and TDD skills \
             to plan a new CLI tool that converts Markdown to HTML. First brainstorm the design, \
             then write tests, then implement."
        )
        .mock();

    // Add all the remote skills
    for skill in skills {
        builder = builder.skill(skill);
    }

    let dev_box = builder.build()?;

    println!("  Box:    {}", dev_box.name);
    println!("  Skills: {}", dev_box.skills.len());
    for skill in &dev_box.skills {
        let kind = match &skill.kind {
            void_box::skill::SkillKind::Agent { .. } => "agent",
            void_box::skill::SkillKind::Remote { .. } => "remote",
            void_box::skill::SkillKind::File { .. } => "file",
            void_box::skill::SkillKind::Mcp { .. } => "mcp",
            void_box::skill::SkillKind::Cli { .. } => "cli",
        };
        println!(
            "    - {} [{}] {}",
            skill.name,
            kind,
            skill.description_text.as_deref().unwrap_or("")
        );
    }
    println!();

    // ---- Run the Box (mock mode -- provisions skills and executes) ----

    println!("--- Running Box (mock mode) ---");
    println!();

    let result = dev_box.run(None).await?;

    println!();
    println!("--- Result ---");
    println!("  Box:     {}", result.box_name);
    println!("  Error:   {}", result.claude_result.is_error);
    println!("  Tokens:  {} in / {} out",
        result.claude_result.input_tokens,
        result.claude_result.output_tokens);
    println!();
    println!("Done. All {} remote skills were fetched and provisioned.", fetched_count);

    Ok(())
}
