use crate::error::FramaCError;

/// Client command sent to Frama-C Server.
#[derive(Debug, Clone)]
pub enum FramaCCommand {
    Get { id: String, request: String, data: serde_json::Value },
    Set { id: String, request: String, data: serde_json::Value },
    Exec { id: String, request: String, data: serde_json::Value },
    Poll,
    Shutdown,
    Kill { id: String },
    SigOn { id: String },
    SigOff { id: String },
}

/// Server response from Frama-C Server.
#[derive(Debug, Clone)]
pub enum FramaCResponse {
    Data { id: String, data: serde_json::Value },
    Error { id: String, msg: String },
    Signal { id: String },
    Rejected { id: String },
    Killed { id: String },
    CmdLineOn,
    CmdLineOff,
}

/// Encode a payload string into a Frama-C Server protocol frame.
///
/// Frame format: `S` + 3 hex digits (≤ 0xFFF bytes),
/// `L` + 7 hex digits (≤ 0xFFFFFFF bytes), or `W` + 15 hex digits.
/// Hex digits are lowercase to match OCaml `Printf.sprintf "%03x"`.
pub fn encode_frame(payload: &str) -> Vec<u8> {
    let len = payload.len();
    let header = if len <= 0xFFF {
        format!("S{:03x}", len)
    } else if len <= 0xFFF_FFFF {
        format!("L{:07x}", len)
    } else {
        format!("W{:015x}", len)
    };
    let mut buf = Vec::with_capacity(header.len() + len);
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(payload.as_bytes());
    buf
}

/// Try to decode one complete frame from a byte buffer.
///
/// Returns `Ok(Some((payload, consumed)))` on success,
/// `Ok(None)` if the buffer is incomplete, or `Err` on format error.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(String, usize)>, FramaCError> {
    if buf.is_empty() {
        return Ok(None);
    }

    let hex_len = match buf[0] {
        b'S' => 3,
        b'L' => 7,
        b'W' => 15,
        other => {
            return Err(FramaCError::InvalidFrame(format!(
                "unexpected frame prefix byte: 0x{:02x}",
                other
            )));
        }
    };

    let header_len = 1 + hex_len;
    if buf.len() < header_len {
        return Ok(None);
    }

    let hex_str = std::str::from_utf8(&buf[1..header_len]).map_err(|e| {
        FramaCError::InvalidFrame(format!("invalid UTF-8 in frame header: {e}"))
    })?;

    let payload_len = usize::from_str_radix(hex_str, 16).map_err(|e| {
        FramaCError::InvalidFrame(format!("invalid hex in frame header '{hex_str}': {e}"))
    })?;

    let total = header_len + payload_len;
    if buf.len() < total {
        return Ok(None);
    }

    let payload = std::str::from_utf8(&buf[header_len..total]).map_err(|e| {
        FramaCError::InvalidFrame(format!("invalid UTF-8 in frame payload: {e}"))
    })?;

    Ok(Some((payload.to_string(), total)))
}

/// Serialize a `FramaCCommand` to a JSON string.
///
/// GET/SET/EXEC produce JSON objects with `cmd`, `id`, `request`, `data` fields.
/// POLL and SHUTDOWN produce JSON string literals `"POLL"` and `"SHUTDOWN"`.
pub fn encode_command(cmd: &FramaCCommand) -> String {
    match cmd {
        FramaCCommand::Get { id, request, data } => {
            serde_json::json!({
                "cmd": "GET", "id": id, "request": request, "data": data
            })
            .to_string()
        }
        FramaCCommand::Set { id, request, data } => {
            serde_json::json!({
                "cmd": "SET", "id": id, "request": request, "data": data
            })
            .to_string()
        }
        FramaCCommand::Exec { id, request, data } => {
            serde_json::json!({
                "cmd": "EXEC", "id": id, "request": request, "data": data
            })
            .to_string()
        }
        FramaCCommand::Poll => "\"POLL\"".to_string(),
        FramaCCommand::Shutdown => "\"SHUTDOWN\"".to_string(),
        FramaCCommand::Kill { id } => {
            serde_json::json!({"cmd": "KILL", "id": id}).to_string()
        }
        FramaCCommand::SigOn { id } => {
            serde_json::json!({"cmd": "SIGON", "id": id}).to_string()
        }
        FramaCCommand::SigOff { id } => {
            serde_json::json!({"cmd": "SIGOFF", "id": id}).to_string()
        }
    }
}

/// Deserialize a JSON string into a `FramaCResponse`.
///
/// Handles both string responses (CMDLINEON/CMDLINEOFF) and
/// object responses (DATA/ERROR/SIGNAL/REJECTED/KILLED).
pub fn decode_response(json_str: &str) -> Result<FramaCResponse, FramaCError> {
    let value: serde_json::Value = serde_json::from_str(json_str)?;

    if let Some(s) = value.as_str() {
        return match s {
            "CMDLINEON" => Ok(FramaCResponse::CmdLineOn),
            "CMDLINEOFF" => Ok(FramaCResponse::CmdLineOff),
            other => Err(FramaCError::UnexpectedResponse(format!(
                "unknown string response: {other}"
            ))),
        };
    }

    if let Some(obj) = value.as_object() {
        let res = obj
            .get("res")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        return match res {
            "DATA" => Ok(FramaCResponse::Data {
                id,
                data: obj.get("data").cloned().unwrap_or(serde_json::Value::Null),
            }),
            "ERROR" => Ok(FramaCResponse::Error {
                id,
                msg: obj
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            }),
            "SIGNAL" => Ok(FramaCResponse::Signal { id }),
            "REJECTED" => Ok(FramaCResponse::Rejected { id }),
            "KILLED" => Ok(FramaCResponse::Killed { id }),
            other => Err(FramaCError::UnexpectedResponse(format!(
                "unknown res type: {other}"
            ))),
        };
    }

    Err(FramaCError::UnexpectedResponse(format!(
        "expected string or object, got: {value}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- encode_frame / decode_frame round-trip ---

    #[test]
    fn frame_roundtrip_small() {
        let payload = r#"{"cmd":"GET","id":"RQ.0","request":"kernel.ast.getFiles","data":null}"#;
        let frame = encode_frame(payload);
        // S prefix for small payloads
        assert_eq!(frame[0], b'S');
        let decoded = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded.0, payload);
        assert_eq!(decoded.1, frame.len());
    }

    #[test]
    fn frame_roundtrip_large() {
        // Create payload > 0xFFF bytes
        let payload = "x".repeat(0x1000);
        let frame = encode_frame(&payload);
        assert_eq!(frame[0], b'L');
        let decoded = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded.0, payload);
    }

    #[test]
    fn decode_frame_incomplete() {
        // Empty buffer
        assert!(decode_frame(b"").unwrap().is_none());
        // Just prefix, no hex
        assert!(decode_frame(b"S").unwrap().is_none());
        // Header complete but payload incomplete
        assert!(decode_frame(b"S00ahel").unwrap().is_none());
    }

    #[test]
    fn decode_frame_invalid_prefix() {
        let result = decode_frame(b"X000hello");
        assert!(result.is_err());
    }

    // --- encode_command ---

    #[test]
    fn encode_get_command() {
        let cmd = FramaCCommand::Get {
            id: "RQ.0".into(),
            request: "kernel.ast.getFiles".into(),
            data: serde_json::Value::Null,
        };
        let json: serde_json::Value = serde_json::from_str(&encode_command(&cmd)).unwrap();
        assert_eq!(json["cmd"], "GET");
        assert_eq!(json["id"], "RQ.0");
        assert_eq!(json["request"], "kernel.ast.getFiles");
        assert!(json["data"].is_null());
    }

    #[test]
    fn encode_poll_command() {
        assert_eq!(encode_command(&FramaCCommand::Poll), "\"POLL\"");
    }

    #[test]
    fn encode_shutdown_command() {
        assert_eq!(encode_command(&FramaCCommand::Shutdown), "\"SHUTDOWN\"");
    }

    #[test]
    fn encode_kill_command() {
        let cmd = FramaCCommand::Kill { id: "RQ.1".into() };
        let json: serde_json::Value = serde_json::from_str(&encode_command(&cmd)).unwrap();
        assert_eq!(json["cmd"], "KILL");
        assert_eq!(json["id"], "RQ.1");
    }

    // --- decode_response ---

    #[test]
    fn decode_cmdlineoff() {
        let resp = decode_response("\"CMDLINEOFF\"").unwrap();
        assert!(matches!(resp, FramaCResponse::CmdLineOff));
    }

    #[test]
    fn decode_cmdlineon() {
        let resp = decode_response("\"CMDLINEON\"").unwrap();
        assert!(matches!(resp, FramaCResponse::CmdLineOn));
    }

    #[test]
    fn decode_data_response() {
        let json = r#"{"res":"DATA","id":"RQ.0","data":["/tmp/test.c"]}"#;
        let resp = decode_response(json).unwrap();
        match resp {
            FramaCResponse::Data { id, data } => {
                assert_eq!(id, "RQ.0");
                assert_eq!(data, serde_json::json!(["/tmp/test.c"]));
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn decode_error_response() {
        let json = r#"{"res":"ERROR","id":"RQ.1","msg":"Expected object, got null: null"}"#;
        let resp = decode_response(json).unwrap();
        match resp {
            FramaCResponse::Error { id, msg } => {
                assert_eq!(id, "RQ.1");
                assert_eq!(msg, "Expected object, got null: null");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn decode_signal_response() {
        let json = r#"{"res":"SIGNAL","id":"RQ.2"}"#;
        let resp = decode_response(json).unwrap();
        assert!(matches!(resp, FramaCResponse::Signal { id } if id == "RQ.2"));
    }

    #[test]
    fn decode_rejected_response() {
        let json = r#"{"res":"REJECTED","id":"RQ.3"}"#;
        let resp = decode_response(json).unwrap();
        assert!(matches!(resp, FramaCResponse::Rejected { id } if id == "RQ.3"));
    }

    #[test]
    fn decode_killed_response() {
        let json = r#"{"res":"KILLED","id":"RQ.4"}"#;
        let resp = decode_response(json).unwrap();
        assert!(matches!(resp, FramaCResponse::Killed { id } if id == "RQ.4"));
    }

    #[test]
    fn decode_data_null() {
        let json = r#"{"res":"DATA","id":"RQ.5","data":null}"#;
        let resp = decode_response(json).unwrap();
        match resp {
            FramaCResponse::Data { data, .. } => assert!(data.is_null()),
            other => panic!("expected Data, got {other:?}"),
        }
    }

    // --- encode_frame + decode_frame with encode_command ---

    #[test]
    fn full_roundtrip_get() {
        let cmd = FramaCCommand::Get {
            id: "RQ.0".into(),
            request: "kernel.ast.getFiles".into(),
            data: serde_json::Value::Null,
        };
        let json = encode_command(&cmd);
        let frame = encode_frame(&json);
        let (decoded_payload, consumed) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
        let decoded_resp_value: serde_json::Value =
            serde_json::from_str(&decoded_payload).unwrap();
        assert_eq!(decoded_resp_value["cmd"], "GET");
    }

    #[test]
    fn full_roundtrip_poll() {
        let json = encode_command(&FramaCCommand::Poll);
        let frame = encode_frame(&json);
        let (decoded, _) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded, "\"POLL\"");
    }
}
