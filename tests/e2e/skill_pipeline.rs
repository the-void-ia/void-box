#![cfg(target_os = "linux")]
//! E2E tests for Skill + VoidBox + Pipeline with real KVM VMs and claudio.
//!
//! These tests verify that:
//! 1. Skills (SKILL.md files) are correctly provisioned into the guest filesystem
//! 2. MCP config (mcp.json) is written correctly
//! 3. claudio discovers provisioned skills and reports them in output
//! 4. Pipeline composition works end-to-end with real VMs
//!
//! Kernel and initramfs are auto-provisioned by [`test_artifacts`] under
//! `--ignored`; `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` are optional overrides.
//! All tests are `#[ignore]`, so a plain `cargo test` never provisions or boots.
//!
//! ```bash
//! cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
//! ```

#[path = "../common/test_artifacts.rs"]
mod test_artifacts;

use void_box::agent_box::VoidBox;
use void_box::pipeline::Pipeline;
use void_box::skill::Skill;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Build a VoidBox on the auto-provisioned test artifacts.
fn build_kvm_box(name: &str, skills: Vec<Skill>, prompt: &str) -> VoidBox {
    let (kernel, initramfs) = test_artifacts::artifacts();

    let mut builder = VoidBox::new(name)
        .kernel(&kernel)
        .initramfs(&initramfs)
        .memory_mb(256)
        .prompt(prompt);

    for skill in skills {
        builder = builder.skill(skill);
    }

    test_artifacts::expect_vm(builder.build(), "skill_pipeline build")
}

// ===========================================================================
// Test 1: VoidBox with a local SKILL.md file
// ===========================================================================

/// Verify that a local SKILL.md is provisioned into the guest and claudio
/// discovers it, reporting the skill name in its output.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_agent_box_with_local_skill() {
    let skills = vec![
        Skill::file("examples/trading_pipeline/skills/financial-data-analysis.md")
            .description("Financial data methodology"),
        Skill::agent("claude-code"),
    ];

    let ab = build_kvm_box("data_analyst", skills, "Analyze AAPL stock data");

    // `run` boots the lazily started VM, so it is the op that can surface a
    // genuine hypervisor absence — gate it as skip-or-fail.
    let Some(result) =
        test_artifacts::vm_start_value(ab.run(None, None).await, "skill_pipeline run")
    else {
        return;
    };

    // Basic checks
    assert_eq!(result.box_name, "data_analyst");
    assert!(!result.agent_result.is_error, "should not be an error");
    assert!(
        !result.agent_result.session_id.is_empty(),
        "session_id should be populated"
    );
    assert!(
        !result.agent_result.tool_calls.is_empty(),
        "should have tool calls"
    );

    // Verify claudio discovered the provisioned skill
    assert!(
        result
            .agent_result
            .result_text
            .contains("financial-data-analysis"),
        "result should mention the provisioned skill name, got: {}",
        result.agent_result.result_text
    );

    eprintln!("PASSED: test_agent_box_with_local_skill");
    eprintln!("  session: {}", result.agent_result.session_id);
    eprintln!("  tools: {}", result.agent_result.tool_calls.len());
    eprintln!("  result: {}", result.agent_result.result_text);
}

// ===========================================================================
// Test 2: VoidBox with multiple skills
// ===========================================================================

/// Verify that multiple SKILL.md files are all provisioned and discovered.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_agent_box_with_multiple_skills() {
    let skills = vec![
        Skill::file("examples/trading_pipeline/skills/financial-data-analysis.md"),
        Skill::file("examples/trading_pipeline/skills/quant-technical-analysis.md"),
        Skill::agent("claude-code"),
    ];

    let ab = build_kvm_box("multi_skill_box", skills, "Analyze and compute indicators");

    let Some(result) =
        test_artifacts::vm_start_value(ab.run(None, None).await, "skill_pipeline run")
    else {
        return;
    };

    assert!(!result.agent_result.is_error);

    // Both skills should be discovered by claudio
    assert!(
        result
            .agent_result
            .result_text
            .contains("financial-data-analysis"),
        "should discover financial-data-analysis skill, got: {}",
        result.agent_result.result_text
    );
    assert!(
        result
            .agent_result
            .result_text
            .contains("quant-technical-analysis"),
        "should discover quant-technical-analysis skill, got: {}",
        result.agent_result.result_text
    );

    eprintln!("PASSED: test_agent_box_with_multiple_skills");
}

// ===========================================================================
// Test 3: VoidBox with MCP skill
// ===========================================================================

/// Verify that MCP config is written to the guest and claudio discovers it.
/// claudio should simulate a tool call to the MCP server.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_agent_box_with_mcp_skill() {
    let skills = vec![
        Skill::mcp("market-data-mcp")
            .description("Market data provider")
            .args(&["--mode", "mock"]),
        Skill::agent("claude-code"),
    ];

    let ab = build_kvm_box("mcp_box", skills, "Fetch market data");

    let Some(result) =
        test_artifacts::vm_start_value(ab.run(None, None).await, "skill_pipeline run")
    else {
        return;
    };

    assert!(!result.agent_result.is_error);

    // claudio should discover the MCP server
    assert!(
        result.agent_result.result_text.contains("market-data-mcp"),
        "should discover MCP server, got: {}",
        result.agent_result.result_text
    );

    // claudio should have simulated an MCP tool call
    let mcp_tools: Vec<_> = result
        .agent_result
        .tool_calls
        .iter()
        .filter(|tc| tc.tool_name.contains("mcp__"))
        .collect();
    assert!(
        !mcp_tools.is_empty(),
        "should have at least one MCP tool call, tools: {:?}",
        result
            .agent_result
            .tool_calls
            .iter()
            .map(|t| &t.tool_name)
            .collect::<Vec<_>>()
    );

    eprintln!("PASSED: test_agent_box_with_mcp_skill");
    eprintln!("  MCP tool calls: {}", mcp_tools.len());
}

// ===========================================================================
// Test 4: VoidBox with mixed skills (file + MCP)
// ===========================================================================

/// Verify that both file skills and MCP servers are provisioned together.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_agent_box_mixed_skills() {
    let skills = vec![
        Skill::file("examples/trading_pipeline/skills/financial-data-analysis.md"),
        Skill::mcp("market-data-mcp").args(&["--mock"]),
        Skill::agent("claude-code"),
    ];

    let ab = build_kvm_box("mixed_box", skills, "Analyze with MCP data");

    let Some(result) =
        test_artifacts::vm_start_value(ab.run(None, None).await, "skill_pipeline run")
    else {
        return;
    };

    assert!(!result.agent_result.is_error);

    // Both skill and MCP should be discovered
    let text = &result.agent_result.result_text;
    assert!(
        text.contains("financial-data-analysis"),
        "should discover file skill: {}",
        text
    );
    assert!(
        text.contains("market-data-mcp"),
        "should discover MCP server: {}",
        text
    );

    eprintln!("PASSED: test_agent_box_mixed_skills");
}

// ===========================================================================
// Test 5: Pipeline with two stages
// ===========================================================================

/// Two-stage pipeline where each Box has its own skills.
/// Verify data flows between stages and both Boxes run successfully.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_pipeline_two_stages_kvm() {
    let box1_skills = vec![
        Skill::file("examples/trading_pipeline/skills/financial-data-analysis.md"),
        Skill::agent("claude-code"),
    ];
    let box2_skills = vec![
        Skill::file("examples/trading_pipeline/skills/quant-technical-analysis.md"),
        Skill::agent("claude-code"),
    ];

    let box1 = build_kvm_box("data_stage", box1_skills, "Collect market data");
    let box2 = build_kvm_box("quant_stage", box2_skills, "Compute indicators");

    let Some(result) = test_artifacts::vm_start_value(
        Pipeline::named("two_stage_test", box1)
            .pipe(box2)
            .run()
            .await,
        "skill_pipeline pipeline run",
    ) else {
        return;
    };

    // Verify pipeline structure
    assert_eq!(result.stages.len(), 2);
    assert_eq!(result.stages[0].box_name, "data_stage");
    assert_eq!(result.stages[1].box_name, "quant_stage");
    assert!(result.success(), "pipeline should succeed");

    // Verify each stage discovered its skill
    assert!(
        result.stages[0]
            .agent_result
            .result_text
            .contains("financial-data-analysis"),
        "stage 1 should have financial skill: {}",
        result.stages[0].agent_result.result_text
    );
    assert!(
        result.stages[1]
            .agent_result
            .result_text
            .contains("quant-technical-analysis"),
        "stage 2 should have quant skill: {}",
        result.stages[1].agent_result.result_text
    );

    eprintln!("PASSED: test_pipeline_two_stages_kvm");
    eprintln!(
        "  Total tokens: {} in / {} out",
        result.total_input_tokens(),
        result.total_output_tokens()
    );
}

// ===========================================================================
// Test 6: VoidBox with input data
// ===========================================================================

/// Verify that input data is written to the guest and the agent receives it.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_agent_box_with_input_data_kvm() {
    let skills = vec![
        Skill::file("examples/trading_pipeline/skills/quant-technical-analysis.md"),
        Skill::agent("claude-code"),
    ];

    let ab = build_kvm_box("input_box", skills, "Process the input data");

    let input = br#"{"symbols": ["AAPL", "NVDA"], "period": "30d"}"#;
    let Some(result) =
        test_artifacts::vm_start_value(ab.run(Some(input), None).await, "skill_pipeline run")
    else {
        return;
    };

    assert_eq!(result.box_name, "input_box");
    assert!(!result.agent_result.is_error);
    assert!(
        !result.agent_result.session_id.is_empty(),
        "should have session_id"
    );

    eprintln!("PASSED: test_agent_box_with_input_data_kvm");
}
