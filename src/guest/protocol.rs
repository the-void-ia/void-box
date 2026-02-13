//! Guest agent protocol definitions.
//!
//! Re-exports all wire-format types from the shared [`void_box_protocol`] crate
//! and adds an async message reader for host-side use (requires tokio).

// Re-export everything from the shared protocol crate so existing
// `use crate::guest::protocol::*` paths continue to work.
pub use void_box_protocol::*;

use crate::Result;

/// Read a complete [`Message`] from an async tokio stream.
///
/// This is the host-side async counterpart of [`Message::read_from_sync`].
pub async fn read_message_async<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Message> {
    let mut header = [0u8; HEADER_SIZE];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|e| crate::Error::Guest(format!("Failed to read message header: {}", e)))?;

    let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let msg_type = MessageType::try_from(header[4])?;

    let mut payload = vec![0u8; length];
    if length > 0 {
        reader
            .read_exact(&mut payload)
            .await
            .map_err(|e| crate::Error::Guest(format!("Failed to read message payload: {}", e)))?;
    }

    Ok(Message { msg_type, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialize_deserialize() {
        let msg = Message {
            msg_type: MessageType::Ping,
            payload: b"hello".to_vec(),
        };

        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();

        assert_eq!(decoded.msg_type, MessageType::Ping);
        assert_eq!(decoded.payload, b"hello");
    }

    #[test]
    fn test_exec_request_serialize() {
        let req = ExecRequest {
            program: "echo".to_string(),
            args: vec!["hello".to_string()],
            stdin: Vec::new(),
            env: Vec::new(),
            working_dir: None,
            timeout_secs: Some(30),
        };

        let json = serde_json::to_string(&req).unwrap();
        let decoded: ExecRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.program, "echo");
        assert_eq!(decoded.args, vec!["hello"]);
    }

    #[test]
    fn test_telemetry_batch_serialize() {
        let batch = TelemetryBatch {
            seq: 1,
            timestamp_ms: 1700000000000,
            system: Some(SystemMetrics {
                cpu_percent: 42.5,
                memory_used_bytes: 1024 * 1024 * 512,
                memory_total_bytes: 1024 * 1024 * 1024,
                net_rx_bytes: 1000,
                net_tx_bytes: 2000,
                procs_running: 3,
                open_fds: 128,
            }),
            processes: vec![ProcessMetrics {
                pid: 1,
                comm: "init".to_string(),
                rss_bytes: 4096,
                cpu_jiffies: 100,
                state: 'S',
            }],
            trace_context: None,
        };

        let json = serde_json::to_vec(&batch).unwrap();
        let decoded: TelemetryBatch = serde_json::from_slice(&json).unwrap();

        assert_eq!(decoded.seq, 1);
        assert_eq!(decoded.system.as_ref().unwrap().cpu_percent, 42.5);
        assert_eq!(decoded.processes.len(), 1);
        assert_eq!(decoded.processes[0].comm, "init");
    }

    #[test]
    fn test_telemetry_message_types() {
        // Verify new message types parse correctly
        let msg = Message {
            msg_type: MessageType::TelemetryData,
            payload: b"{}".to_vec(),
        };
        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();
        assert_eq!(decoded.msg_type, MessageType::TelemetryData);

        let msg = Message {
            msg_type: MessageType::SubscribeTelemetry,
            payload: vec![],
        };
        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();
        assert_eq!(decoded.msg_type, MessageType::SubscribeTelemetry);
    }

    #[test]
    fn test_exec_response() {
        let resp = ExecResponse::success(b"output".to_vec(), b"error".to_vec(), 0, 100);
        assert!(resp.error.is_none());
        assert_eq!(resp.exit_code, 0);

        let err_resp = ExecResponse::error("failed".to_string());
        assert!(err_resp.error.is_some());
        assert_eq!(err_resp.exit_code, -1);
    }
}
