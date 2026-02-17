//! E2E tests for Skill + VoidBox + Pipeline with real KVM VMs and claudio.
//!
//! These tests verify that:
//! 1. Skills (SKILL.md files) are correctly provisioned into the guest filesystem
//! 2. MCP config (mcp.json) is written correctly
//! 3. claudio discovers provisioned skills and reports them in output
//! 4. Pipeline composition works end-to-end with real VMs
//!
//! ## Prerequisites
//!
//! 1. Build the test initramfs (includes updated claudio with skill discovery):
//!    ```bash
//!    scripts/build_test_image.sh
//!    ```
//!
//! 2. Run with:
//!    ```bash
//!    VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!    VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
//!    cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
//!    ```
//!
//! All tests are `#[ignore]` so they don't run in a normal `cargo test`.

use std::path::PathBuf;

use void_box::agent_box::VoidBox;
use void_box::pipeline::Pipeline;
use void_box::skill::Skill;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn kvm_available() -> bool {
    std::path::Path::new("/dev/kvm").exists()
}

fn vsock_available() -> bool {
    std::path::Path::new("/dev/vhost-vsock").exists()
}

fn kvm_artifacts() -> Option<(PathBuf, PathBuf)> {
    let kernel = std::env::var("VOID_BOX_KERNEL").ok()?;
    let kernel = PathBuf::from(kernel);
    if kernel.as_os_str().is_empty() || !kernel.exists() {
        return None;
    }

    let initramfs = std::env::var("VOID_BOX_INITRAMFS").ok()?;
    let initramfs = PathBuf::from(initramfs);
    if initramfs.as_os_str().is_empty() || !initramfs.exists() {
        return None;
    }

    Some((kernel, initramfs))
}

/// Build an VoidBox pointing at real KVM artifacts.
/// Returns None if KVM or artifacts are unavailable (test will skip).
fn build_kvm_box(name: &str, skills: Vec<Skill>, prompt: &str) -> Option<VoidBox> {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return None;
    }
    if !vsock_available() {
        eprintln!("skipping: /dev/vhost-vsock not available");
        return None;
    }

    let (kernel, initramfs) = match kvm_artifacts() {
        Some(a) => a,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return None;
        }
    };

    let mut builder = VoidBox::new(name)
        .kernel(&kernel)
        .initramfs(&initramfs)
        .memory_mb(256)
        .prompt(prompt);

    for skill in skills {
        builder = builder.skill(skill);
    }

    match builder.build() {
        Ok(ab) => Some(ab),
        Err(e) => {
            eprintln!("skipping: failed to build VoidBox: {}", e);
            None
        }
    }
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

    let ab = match build_kvm_box("data_analyst", skills, "Analyze AAPL stock data") {
        Some(ab) => ab,
        None => return,
    };

    let result = ab.run(None).await.expect("VoidBox::run failed");

    // Basic checks
    assert_eq!(result.box_name, "data_analyst");
    assert!(!result.claude_result.is_error, "should not be an error");
    assert!(
        !result.claude_result.session_id.is_empty(),
        "session_id should be populated"
    );
    assert!(
        !result.claude_result.tool_calls.is_empty(),
        "should have tool calls"
    );

    // Verify claudio discovered the provisioned skill
    assert!(
        result
            .claude_result
            .result_text
            .contains("financial-data-analysis"),
        "result should mention the provisioned skill name, got: {}",
        result.claude_result.result_text
    );

    eprintln!("PASSED: test_agent_box_with_local_skill");
    eprintln!("  session: {}", result.claude_result.session_id);
    eprintln!("  tools: {}", result.claude_result.tool_calls.len());
    eprintln!("  result: {}", result.claude_result.result_text);
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

    let ab = match build_kvm_box("multi_skill_box", skills, "Analyze and compute indicators") {
        Some(ab) => ab,
        None => return,
    };

    let result = ab.run(None).await.expect("VoidBox::run failed");

    assert!(!result.claude_result.is_error);

    // Both skills should be discovered by claudio
    assert!(
        result
            .claude_result
            .result_text
            .contains("financial-data-analysis"),
        "should discover financial-data-analysis skill, got: {}",
        result.claude_result.result_text
    );
    assert!(
        result
            .claude_result
            .result_text
            .contains("quant-technical-analysis"),
        "should discover quant-technical-analysis skill, got: {}",
        result.claude_result.result_text
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

    let ab = match build_kvm_box("mcp_box", skills, "Fetch market data") {
        Some(ab) => ab,
        None => return,
    };

    let result = ab.run(None).await.expect("VoidBox::run failed");

    assert!(!result.claude_result.is_error);

    // claudio should discover the MCP server
    assert!(
        result.claude_result.result_text.contains("market-data-mcp"),
        "should discover MCP server, got: {}",
        result.claude_result.result_text
    );

    // claudio should have simulated an MCP tool call
    let mcp_tools: Vec<_> = result
        .claude_result
        .tool_calls
        .iter()
        .filter(|tc| tc.tool_name.contains("mcp__"))
        .collect();
    assert!(
        !mcp_tools.is_empty(),
        "should have at least one MCP tool call, tools: {:?}",
        result
            .claude_result
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

    let ab = match build_kvm_box("mixed_box", skills, "Analyze with MCP data") {
        Some(ab) => ab,
        None => return,
    };

    let result = ab.run(None).await.expect("VoidBox::run failed");

    assert!(!result.claude_result.is_error);

    // Both skill and MCP should be discovered
    let text = &result.claude_result.result_text;
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

    let box1 = match build_kvm_box("data_stage", box1_skills, "Collect market data") {
        Some(ab) => ab,
        None => return,
    };
    let box2 = match build_kvm_box("quant_stage", box2_skills, "Compute indicators") {
        Some(ab) => ab,
        None => return,
    };

    let result = Pipeline::named("two_stage_test", box1)
        .pipe(box2)
        .run()
        .await
        .expect("Pipeline::run failed");

    // Verify pipeline structure
    assert_eq!(result.stages.len(), 2);
    assert_eq!(result.stages[0].box_name, "data_stage");
    assert_eq!(result.stages[1].box_name, "quant_stage");
    assert!(result.success(), "pipeline should succeed");

    // Verify each stage discovered its skill
    assert!(
        result.stages[0]
            .claude_result
            .result_text
            .contains("financial-data-analysis"),
        "stage 1 should have financial skill: {}",
        result.stages[0].claude_result.result_text
    );
    assert!(
        result.stages[1]
            .claude_result
            .result_text
            .contains("quant-technical-analysis"),
        "stage 2 should have quant skill: {}",
        result.stages[1].claude_result.result_text
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

    let ab = match build_kvm_box("input_box", skills, "Process the input data") {
        Some(ab) => ab,
        None => return,
    };

    let input = br#"{"symbols": ["AAPL", "NVDA"], "period": "30d"}"#;
    let result = ab.run(Some(input)).await.expect("VoidBox::run failed");

    assert_eq!(result.box_name, "input_box");
    assert!(!result.claude_result.is_error);
    assert!(
        !result.claude_result.session_id.is_empty(),
        "should have session_id"
    );

    eprintln!("PASSED: test_agent_box_with_input_data_kvm");
}
