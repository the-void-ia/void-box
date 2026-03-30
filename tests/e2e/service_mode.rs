/// End-to-end test for `agent.mode: service` lifecycle.
///
/// Exercises the full stack: daemon API -> runtime -> VoidBox -> VM -> guest-agent.
/// Verifies that a service agent publishes output while still running, that
/// messaging/MCP is active, and that cancel works after publication.
#[path = "../common/vm_preflight.rs"]
mod vm_preflight;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn kvm_artifacts() -> Option<(PathBuf, PathBuf)> {
    let kernel = std::env::var("VOID_BOX_KERNEL").ok()?;
    let initramfs = std::env::var("VOID_BOX_INITRAMFS").ok()?;
    Some((PathBuf::from(kernel), PathBuf::from(initramfs)))
}

fn http_request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).expect("connect failed");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();

    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);

    let (head, body) = response.split_once("\r\n\r\n").unwrap_or(("", ""));
    let status = head.lines().next().unwrap_or("").to_string();
    (status, body.to_string())
}

fn try_http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: &str,
) -> Option<(String, String)> {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return None;
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or(("", ""));
    let status = head.lines().next().unwrap_or("").to_string();
    Some((status, body.to_string()))
}

fn wait_for_http_ok(addr: SocketAddr, path: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some((status, _)) = try_http_request(addr, "GET", path, "") {
            if status.contains("200") {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("daemon did not become ready for {}", path);
}

fn start_daemon(kernel: &std::path::Path, initramfs: &std::path::Path) -> SocketAddr {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = std::net::TcpListener::bind(addr).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let kernel = kernel.to_path_buf();
    let initramfs = initramfs.to_path_buf();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            std::env::set_var("VOID_BOX_KERNEL", kernel.to_str().unwrap());
            std::env::set_var("VOID_BOX_INITRAMFS", initramfs.to_str().unwrap());
            void_box::daemon::serve(addr).await.unwrap();
        });
    });

    wait_for_http_ok(addr, "/v1/health", Duration::from_secs(10));
    addr
}

fn wait_for_terminal(addr: SocketAddr, run_id: &str, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let (status, body) = http_request(addr, "GET", &format!("/v1/runs/{run_id}"), "");
        if status.contains("200") {
            let run: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let run_status = run["status"].as_str().unwrap_or("unknown");
            let is_terminal =
                run_status == "succeeded" || run_status == "failed" || run_status == "cancelled";
            if is_terminal {
                return run;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    panic!("run {} did not become terminal within timeout", run_id);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs + ANTHROPIC_API_KEY"]
async fn e2e_service_mode_output_publication() {
    if vm_preflight::require_kvm_usable().is_err() {
        eprintln!("skipping: KVM not available");
        return;
    }
    if vm_preflight::require_vsock_usable().is_err() {
        eprintln!("skipping: vsock not available");
        return;
    }
    let Some((kernel, initramfs)) = kvm_artifacts() else {
        eprintln!("skipping: VOID_BOX_KERNEL / VOID_BOX_INITRAMFS not set");
        return;
    };
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("skipping: ANTHROPIC_API_KEY not set");
        return;
    }

    let tmpdir = tempfile::tempdir().expect("tmpdir");
    let spec_path = tmpdir.path().join("service_test.yaml");

    let spec_yaml = r#"
kind: agent
name: service-mode-e2e
agent:
  skills:
    - "agent:claude-code"
  messaging:
    enabled: true
  mode: service
  output_file: /workspace/output.json
  prompt: |
    Do these steps exactly in order:

    1. Verify the Void MCP/sidecar toolchain is alive by using one tool or command
       from the void messaging integration.
    2. Write this exact JSON object to /workspace/output.json:
       {"result":"service-mode-test","status":"complete"}
    3. After writing the file, keep the service alive for at least 30 seconds.
       Use a concrete long-running wait command so the run remains active.
    4. Do not delete or rewrite /workspace/output.json.
sandbox:
  memory_mb: 3072
  network: true
  mode: auto
"#;

    std::fs::write(&spec_path, spec_yaml).unwrap();

    let addr = start_daemon(&kernel, &initramfs);
    eprintln!("Daemon started on {addr}");

    let create_body = serde_json::json!({
        "file": spec_path.to_str().unwrap(),
    })
    .to_string();

    let (status, body) = http_request(addr, "POST", "/v1/runs", &create_body);
    assert!(status.contains("200"), "create_run failed: {status} {body}");

    let create_resp: serde_json::Value =
        serde_json::from_str(&body).expect("parse create response");
    let run_id = create_resp["run_id"].as_str().expect("run_id missing");
    eprintln!("Created run: {run_id}");

    let mut output_ready = false;
    let deadline = Instant::now() + Duration::from_secs(180);

    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));

        let (status, body) = http_request(addr, "GET", &format!("/v1/runs/{run_id}"), "");
        if !status.contains("200") {
            eprintln!("GET run failed: {status}");
            continue;
        }

        let run: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let ready = run["output_ready"].as_bool().unwrap_or(false);
        let run_status = run["status"].as_str().unwrap_or("unknown");
        let event_count = run["events"].as_array().map(|a| a.len()).unwrap_or(0);
        eprintln!(
            "Poll: status={}, output_ready={}, events={}",
            run_status, ready, event_count
        );

        if ready {
            output_ready = true;
            assert_eq!(
                run_status, "running",
                "run should still be running when output_ready becomes true"
            );
            assert!(run["report"].is_object(), "report should be present");
            assert!(run["sidecar"].is_object(), "sidecar should be present");
            assert!(
                run["artifact_publication"].is_object(),
                "artifact_publication should be present"
            );

            // Verify output-file endpoint returns actual data
            let stage = run["report"]["name"].as_str().unwrap_or("service-mode-e2e");
            let (of_status, of_body) = http_request(
                addr,
                "GET",
                &format!("/v1/runs/{run_id}/stages/{stage}/output-file"),
                "",
            );
            assert!(
                of_status.contains("200"),
                "output-file should return 200, got: {of_status}"
            );
            assert!(
                !of_body.is_empty(),
                "output-file should return non-empty body"
            );
            eprintln!("output-file returned {} bytes", of_body.len());

            let (cancel_status, cancel_body) =
                http_request(addr, "POST", &format!("/v1/runs/{run_id}/cancel"), "{}");
            assert!(
                cancel_status.contains("200"),
                "cancel failed: {cancel_status} {cancel_body}"
            );

            let final_run = wait_for_terminal(addr, run_id, Duration::from_secs(60));
            let final_status = final_run["status"].as_str().unwrap_or("unknown");
            assert!(
                final_status == "succeeded"
                    || final_status == "failed"
                    || final_status == "cancelled",
                "run should be terminal after cancel, got: {final_status}"
            );
            assert!(
                final_run["report"].is_object(),
                "published report should remain present after terminalization"
            );
            break;
        }

        let is_terminal =
            run_status == "succeeded" || run_status == "failed" || run_status == "cancelled";
        if is_terminal {
            eprintln!("Run terminated without output_ready: {run_status}");
            eprintln!(
                "Run body: {}",
                serde_json::to_string_pretty(&run).unwrap_or_default()
            );
            break;
        }
    }

    assert!(output_ready, "output_ready never became true within 180s");
    eprintln!("PASSED: e2e_service_mode_output_publication");
}
