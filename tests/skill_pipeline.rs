//! Test battery for the Skill + AgentBox + Pipeline stack.
//!
//! Covers:
//! - Skill provisioning (all 5 types: agent, file, mcp, cli, remote)
//! - Remote skill fetching (live + fallback)
//! - Pipeline composition (single, multi-stage)
//! - Trading pipeline integration (mock mode)
//!
//! All tests run with mock sandbox (no KVM required) unless marked `#[ignore]`.

use void_box::agent_box::AgentBox;
use void_box::pipeline::Pipeline;
use void_box::skill::Skill;

// ─── Skill Provisioning ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_provision_local_skill_file() {
    let skill = Skill::file("examples/trading_pipeline/skills/financial-data-analysis.md")
        .description("Financial data methodology");
    let reasoning = Skill::agent("claude-code");

    let ab = AgentBox::new("data_analyst")
        .skill(skill)
        .skill(reasoning)
        .prompt("Analyze data")
        .mock()
        .build()
        .unwrap();

    assert_eq!(ab.skills.len(), 2);

    let result = ab.run(None).await.unwrap();
    assert_eq!(result.box_name, "data_analyst");
    assert!(!result.claude_result.is_error);
}

#[tokio::test]
async fn test_provision_mcp_skill() {
    let mcp = Skill::mcp("market-data-mcp")
        .description("Market data provider")
        .args(&["--mode", "mock"])
        .env("API_KEY", "test-key");
    let reasoning = Skill::agent("claude-code");

    let ab = AgentBox::new("mcp_box")
        .skill(mcp)
        .skill(reasoning)
        .prompt("Use MCP tools")
        .mock()
        .build()
        .unwrap();

    assert_eq!(ab.skills.len(), 2);

    let result = ab.run(None).await.unwrap();
    assert_eq!(result.box_name, "mcp_box");
    assert!(!result.claude_result.is_error);
}

#[tokio::test]
async fn test_provision_cli_skill() {
    let cli = Skill::cli("jq")
        .description("JSON processor");
    let reasoning = Skill::agent("claude-code");

    let ab = AgentBox::new("cli_box")
        .skill(cli)
        .skill(reasoning)
        .prompt("Process JSON")
        .mock()
        .build()
        .unwrap();

    assert_eq!(ab.skills.len(), 2);

    let result = ab.run(None).await.unwrap();
    assert_eq!(result.box_name, "cli_box");
}

#[tokio::test]
async fn test_provision_mixed_skills() {
    // All 5 skill types in one Box
    let agent = Skill::agent("claude-code");
    let file = Skill::file("examples/trading_pipeline/skills/quant-technical-analysis.md");
    let mcp = Skill::mcp("data-server").args(&["--port", "8080"]);
    let cli = Skill::cli("python3");
    // Remote will hit fallback (nonexistent repo) -- should not fail
    let remote = Skill::remote("nonexistent-test-org/nonexistent-repo/fake-skill");

    let ab = AgentBox::new("mixed_box")
        .skill(agent)
        .skill(file)
        .skill(mcp)
        .skill(cli)
        .skill(remote)
        .prompt("Do everything")
        .mock()
        .build()
        .unwrap();

    assert_eq!(ab.skills.len(), 5);

    // Should succeed even though remote fetch will 404 (fallback kicks in)
    let result = ab.run(None).await.unwrap();
    assert_eq!(result.box_name, "mixed_box");
    assert!(!result.claude_result.is_error);
}

#[tokio::test]
async fn test_provision_remote_skill_fallback() {
    // Intentionally nonexistent skill -- fetch will 404
    let remote = Skill::remote("nonexistent-org-12345/nonexistent-repo-67890/no-such-skill")
        .description("This will fail to fetch");
    let reasoning = Skill::agent("claude-code");

    let ab = AgentBox::new("fallback_box")
        .skill(remote)
        .skill(reasoning)
        .prompt("Try with fallback")
        .mock()
        .build()
        .unwrap();

    // The Box should run successfully -- fallback content is written instead
    let result = ab.run(None).await.unwrap();
    assert_eq!(result.box_name, "fallback_box");
    assert!(!result.claude_result.is_error);
}

// ─── Remote Fetching (live network, ignored) ────────────────────────────────

#[tokio::test]
#[ignore] // Requires network access
async fn test_remote_skill_provision_live() {
    let brainstorm = Skill::remote("obra/superpowers/brainstorming")
        .description("Brainstorming methodology");
    let reasoning = Skill::agent("claude-code");

    let ab = AgentBox::new("live_fetch_box")
        .skill(brainstorm)
        .skill(reasoning)
        .prompt("Brainstorm ideas")
        .mock()
        .build()
        .unwrap();

    let result = ab.run(None).await.unwrap();
    assert_eq!(result.box_name, "live_fetch_box");
    assert!(!result.claude_result.is_error);
}

#[tokio::test]
#[ignore] // Requires network access
async fn test_remote_skill_url_patterns() {
    // 3-part: owner/repo/skill-name
    let s3 = Skill::remote("obra/superpowers/brainstorming");
    let content = s3.fetch_remote_content().await.unwrap();
    assert!(
        content.contains("Brainstorming"),
        "3-part fetch should return brainstorming skill"
    );

    // 2-part: owner/repo (fetches root SKILL.md)
    // vercel-labs/skills doesn't have a root SKILL.md, so this tests the URL pattern
    let s2 = Skill::remote("obra/superpowers");
    assert_eq!(
        s2.remote_url().unwrap(),
        "https://raw.githubusercontent.com/obra/superpowers/main/SKILL.md"
    );
}

// ─── Pipeline Composition ───────────────────────────────────────────────────

#[tokio::test]
async fn test_pipeline_single_stage() {
    let reasoning = Skill::agent("claude-code");

    let single_box = AgentBox::new("solo")
        .skill(reasoning)
        .prompt("Do one thing")
        .mock()
        .build()
        .unwrap();

    let result = Pipeline::from(single_box).run().await.unwrap();

    assert_eq!(result.stages.len(), 1);
    assert_eq!(result.stages[0].box_name, "solo");
    assert!(result.success());
}

#[tokio::test]
async fn test_pipeline_three_stages() {
    let make_box = |name: &str, prompt: &str| -> AgentBox {
        AgentBox::new(name)
            .skill(Skill::agent("claude-code"))
            .prompt(prompt)
            .mock()
            .build()
            .unwrap()
    };

    let box1 = make_box("stage_1", "First step");
    let box2 = make_box("stage_2", "Second step");
    let box3 = make_box("stage_3", "Third step");

    let result = Pipeline::named("three_stage_test", box1)
        .pipe(box2)
        .pipe(box3)
        .run()
        .await
        .unwrap();

    assert_eq!(result.name, "three_stage_test");
    assert_eq!(result.stages.len(), 3);
    assert_eq!(result.stages[0].box_name, "stage_1");
    assert_eq!(result.stages[1].box_name, "stage_2");
    assert_eq!(result.stages[2].box_name, "stage_3");
    assert!(result.success());
}

#[tokio::test]
async fn test_pipeline_result_accessors() {
    let box1 = AgentBox::new("a")
        .skill(Skill::agent("claude-code"))
        .prompt("Step A")
        .mock()
        .build()
        .unwrap();

    let box2 = AgentBox::new("b")
        .skill(Skill::agent("claude-code"))
        .prompt("Step B")
        .mock()
        .build()
        .unwrap();

    let result = Pipeline::named("accessor_test", box1)
        .pipe(box2)
        .run()
        .await
        .unwrap();

    // Mock sandbox produces 0 tokens/cost, but accessors should work
    assert_eq!(result.total_cost_usd(), 0.0);
    assert_eq!(result.total_input_tokens(), 0);
    assert_eq!(result.total_output_tokens(), 0);
    assert_eq!(result.total_tool_calls(), 0);
    assert!(result.success());
    assert_eq!(result.stages.len(), 2);
}

#[tokio::test]
async fn test_pipeline_len() {
    let box1 = AgentBox::new("x")
        .skill(Skill::agent("claude-code"))
        .prompt("X")
        .mock()
        .build()
        .unwrap();
    let box2 = AgentBox::new("y")
        .skill(Skill::agent("claude-code"))
        .prompt("Y")
        .mock()
        .build()
        .unwrap();

    let p = Pipeline::named("len_test", box1).pipe(box2);
    assert_eq!(p.len(), 2);
    assert!(!p.is_empty());
}

// ─── Trading Pipeline Integration ───────────────────────────────────────────

#[tokio::test]
async fn test_trading_pipeline_mock() {
    // Reproduces the trading_pipeline example as an automated test
    let reasoning = Skill::agent("claude-code");

    let data_box = AgentBox::new("data_analyst")
        .skill(Skill::file("examples/trading_pipeline/skills/financial-data-analysis.md"))
        .skill(reasoning.clone())
        .prompt("Fetch 30 days of OHLCV data for AAPL, NVDA, MSFT, GOOGL")
        .mock()
        .build()
        .unwrap();

    let quant_box = AgentBox::new("quant_analyst")
        .skill(Skill::file("examples/trading_pipeline/skills/quant-technical-analysis.md"))
        .skill(reasoning.clone())
        .prompt("Compute technical indicators for each symbol")
        .mock()
        .build()
        .unwrap();

    let sentiment_box = AgentBox::new("research_analyst")
        .skill(reasoning.clone())
        .prompt("Assess market sentiment for each symbol")
        .mock()
        .build()
        .unwrap();

    let strategy_box = AgentBox::new("portfolio_strategist")
        .skill(Skill::file("examples/trading_pipeline/skills/portfolio-risk-management.md"))
        .skill(reasoning.clone())
        .memory_mb(512)
        .prompt("Generate trade recommendations with risk management")
        .mock()
        .build()
        .unwrap();

    let result = Pipeline::named("trading_analysis", data_box)
        .pipe(quant_box)
        .pipe(sentiment_box)
        .pipe(strategy_box)
        .run()
        .await
        .unwrap();

    // Verify pipeline structure
    assert_eq!(result.name, "trading_analysis");
    assert_eq!(result.stages.len(), 4);
    assert!(result.success());

    // Verify stage names in order
    assert_eq!(result.stages[0].box_name, "data_analyst");
    assert_eq!(result.stages[1].box_name, "quant_analyst");
    assert_eq!(result.stages[2].box_name, "research_analyst");
    assert_eq!(result.stages[3].box_name, "portfolio_strategist");
}

// ─── AgentBox with Input Data ───────────────────────────────────────────────

#[tokio::test]
async fn test_agent_box_with_input_data() {
    let reasoning = Skill::agent("claude-code");

    let ab = AgentBox::new("processor")
        .skill(reasoning)
        .prompt("Process the input data")
        .mock()
        .build()
        .unwrap();

    let input = br#"{"symbols": ["AAPL"], "data": []}"#;
    let result = ab.run(Some(input)).await.unwrap();

    assert_eq!(result.box_name, "processor");
    assert!(!result.claude_result.is_error);
}

// ─── Skill URL Generation ───────────────────────────────────────────────────

#[test]
fn test_skill_remote_url_three_part() {
    let s = Skill::remote("obra/superpowers/brainstorming");
    assert_eq!(
        s.remote_url().unwrap(),
        "https://raw.githubusercontent.com/obra/superpowers/main/skills/brainstorming/SKILL.md"
    );
    assert_eq!(s.name, "brainstorming");
}

#[test]
fn test_skill_remote_url_two_part() {
    let s = Skill::remote("vercel-labs/skills");
    assert_eq!(
        s.remote_url().unwrap(),
        "https://raw.githubusercontent.com/vercel-labs/skills/main/SKILL.md"
    );
    assert_eq!(s.name, "skills");
}

#[test]
fn test_skill_remote_url_one_part_returns_none() {
    let s = Skill::remote("justname");
    assert!(s.remote_url().is_none());
}

#[test]
fn test_skill_descriptions() {
    let s = Skill::remote("obra/superpowers/brainstorming")
        .description("Brainstorming methodology");
    assert_eq!(s.description_text.as_deref(), Some("Brainstorming methodology"));

    let s2 = Skill::agent("claude-code");
    assert!(s2.description_text.is_none());
}

#[test]
fn test_skill_mcp_config_entry() {
    let s = Skill::mcp("data-server")
        .args(&["--port", "8080"])
        .env("TOKEN", "abc");

    let entry = s.mcp_config_entry().unwrap();
    assert_eq!(entry["command"], "data-server");
    assert_eq!(entry["args"][0], "--port");
    assert_eq!(entry["args"][1], "8080");
    assert_eq!(entry["env"]["TOKEN"], "abc");
}

#[test]
fn test_skill_mcp_config_entry_no_env() {
    let s = Skill::mcp("simple-server");
    let entry = s.mcp_config_entry().unwrap();
    assert_eq!(entry["command"], "simple-server");
    assert!(entry.get("env").is_none());
}

#[test]
fn test_non_mcp_skill_has_no_config_entry() {
    assert!(Skill::agent("claude").mcp_config_entry().is_none());
    assert!(Skill::cli("jq").mcp_config_entry().is_none());
    assert!(Skill::remote("a/b/c").mcp_config_entry().is_none());
    assert!(Skill::file("test.md").mcp_config_entry().is_none());
}
