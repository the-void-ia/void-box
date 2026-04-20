//! Divan micro-benchmarks for host-side hot paths on the startup path.
//!
//! These complement the `voidbox-startup-bench` wall-clock harness: the
//! harness measures end-to-end VM lifecycle (seconds), these measure the
//! pure-compute slices the profile showed on every boot (nanoseconds to
//! microseconds). Their job is **regression detection** — any of these
//! blowing up signals a subtle perf regression that the wall-clock loop
//! would drown out behind the dominant kernel / guest-agent waits.
//!
//! Hot paths chosen from profiling against the startup harness:
//! - `Message::serialize` / `Message::deserialize` — framed on every RPC
//!   (handshake Ping, ExecRequest, ExecResponse, streaming chunks).
//! - `Message::read_from_sync` — framed read on every response.
//! - `ExecRequest` serde_json encode — once per `exec()`.
//! - `VoidBoxConfig::kernel_cmdline` — once per cold boot.
//! - `getrandom::fill(&mut [u8; 32])` — session secret per cold boot.
//!
//! Run with: `cargo bench --bench startup`

use divan::Bencher;
#[cfg(target_os = "linux")]
use void_box::VoidBoxConfig;
use void_box_protocol::{ExecRequest, Message, MessageType, PROTOCOL_VERSION};

fn main() {
    divan::main();
}

// ---------------------------------------------------------------------------
// Protocol framing (`void_box_protocol::Message`)
// ---------------------------------------------------------------------------

/// Ping payload is [32 bytes session secret][4 bytes protocol version].
/// This is the first message on every boot.
fn ping_message() -> Message {
    let mut payload = vec![0u8; 32];
    payload.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    Message {
        msg_type: MessageType::Ping,
        payload,
    }
}

/// ExecRequest bytes for "sh -c :" — matches the harness's minimum exec.
fn exec_request_bytes() -> Vec<u8> {
    serde_json::to_vec(&ExecRequest {
        program: "sh".into(),
        args: vec!["-c".into(), ":".into()],
        stdin: Vec::new(),
        env: Vec::new(),
        working_dir: None,
        timeout_secs: None,
    })
    .expect("exec request serializes")
}

#[divan::bench]
fn message_serialize_ping(bencher: Bencher) {
    let msg = ping_message();
    bencher.bench_local(|| divan::black_box(divan::black_box(&msg).serialize()));
}

#[divan::bench(args = [64, 1024, 16 * 1024])]
fn message_serialize_payload(bencher: Bencher, size: usize) {
    let msg = Message {
        msg_type: MessageType::ExecOutputChunk,
        payload: vec![0xABu8; size],
    };
    bencher.bench_local(|| divan::black_box(divan::black_box(&msg).serialize()));
}

#[divan::bench]
fn message_deserialize_ping(bencher: Bencher) {
    let bytes = ping_message().serialize();
    bencher
        .bench_local(|| divan::black_box(Message::deserialize(divan::black_box(&bytes)).unwrap()));
}

#[divan::bench(args = [64, 1024, 16 * 1024])]
fn message_read_from_sync(bencher: Bencher, size: usize) {
    let msg = Message {
        msg_type: MessageType::ExecOutputChunk,
        payload: vec![0xCDu8; size],
    };
    let bytes = msg.serialize();
    bencher.bench_local(|| {
        let mut cursor = std::io::Cursor::new(divan::black_box(&bytes));
        divan::black_box(Message::read_from_sync(&mut cursor).unwrap())
    });
}

// ---------------------------------------------------------------------------
// Exec request JSON encoding
// ---------------------------------------------------------------------------

#[divan::bench]
fn exec_request_to_json(bencher: Bencher) {
    let req = ExecRequest {
        program: "sh".into(),
        args: vec!["-c".into(), ":".into()],
        stdin: Vec::new(),
        env: Vec::new(),
        working_dir: None,
        timeout_secs: None,
    };
    bencher.bench_local(|| divan::black_box(serde_json::to_vec(divan::black_box(&req)).unwrap()));
}

#[divan::bench]
fn exec_request_from_json(bencher: Bencher) {
    let bytes = exec_request_bytes();
    bencher.bench_local(|| {
        divan::black_box(serde_json::from_slice::<ExecRequest>(divan::black_box(&bytes)).unwrap())
    });
}

// ---------------------------------------------------------------------------
// VM boot-time host-side compute
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[divan::bench]
fn voidbox_config_kernel_cmdline(bencher: Bencher) {
    // Matches what the startup harness builds: minimal-but-real config.
    // Linux-only: VoidBoxConfig lives in the KVM backend which is not
    // compiled on macOS (where VZ is used instead).
    let config = VoidBoxConfig::new()
        .memory_mb(1024)
        .vcpus(1)
        .kernel("/boot/vmlinuz")
        .initramfs("/tmp/rootfs.cpio.gz")
        .enable_vsock(true);
    bencher.bench_local(|| divan::black_box(divan::black_box(&config).kernel_cmdline()));
}

#[divan::bench]
fn session_secret_getrandom(bencher: Bencher) {
    bencher.bench_local(|| {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).unwrap();
        divan::black_box(secret)
    });
}

/// Hex encoding of the 32-byte session secret into the kernel cmdline —
/// the existing implementation uses a 32× `format!("{:02x}", b)` loop.
/// This bench ensures any future "just faster hex" tweak is measured.
#[divan::bench]
fn session_secret_hex_encode(bencher: Bencher) {
    let secret = [0x5Au8; 32];
    bencher.bench_local(|| {
        let hex: String = divan::black_box(&secret)
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        divan::black_box(hex)
    });
}
