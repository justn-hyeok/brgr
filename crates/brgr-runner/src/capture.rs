//! Bounded capture of a child process's output streams.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    task::JoinError,
};

use crate::{
    CAPTURE_OVERHEAD_BYTES, HarnessManifest, JSONL_METADATA_SLACK_BYTES,
    JSONL_TRANSPORT_LIMIT_BYTES, ResultSource, RunnerError,
};

pub(crate) type CaptureTask = tokio::task::JoinHandle<Result<(Vec<u8>, bool), std::io::Error>>;

pub(crate) fn start_capture(
    child: &mut tokio::process::Child,
    manifest: &HarnessManifest,
) -> Result<(CaptureTask, CaptureTask, Arc<AtomicBool>), RunnerError> {
    let stdout = child
        .stdout
        .take()
        .ok_or(RunnerError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(RunnerError::MissingPipe("stderr"))?;
    let limit = manifest.result.max_bytes;
    let overflow = Arc::new(AtomicBool::new(false));
    Ok((
        tokio::spawn(capture_stdout(
            stdout,
            limit,
            manifest.result.source.clone(),
            Arc::clone(&overflow),
        )),
        tokio::spawn(capture_bounded(stderr, limit, Arc::clone(&overflow))),
        overflow,
    ))
}

async fn capture_bounded<R>(
    reader: R,
    limit: u64,
    overflow: Arc<AtomicBool>,
) -> Result<(Vec<u8>, bool), std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let capture = read_bounded(reader, limit).await?;
    if capture.1 {
        overflow.store(true, Ordering::Relaxed);
    }
    Ok(capture)
}

async fn capture_stdout<R>(
    reader: R,
    limit: u64,
    source: ResultSource,
    overflow: Arc<AtomicBool>,
) -> Result<(Vec<u8>, bool), std::io::Error>
where
    R: AsyncRead + Unpin,
{
    if source == ResultSource::JsonlAssistantFinal {
        read_jsonl_semantic(reader, limit, JSONL_TRANSPORT_LIMIT_BYTES, overflow).await
    } else {
        capture_bounded(reader, limit, overflow).await
    }
}

// JSONL update events repeat growing message bodies. Keep only events used by
// result and model verification while bounding the bytes read and retained.
pub(crate) async fn read_jsonl_semantic<R>(
    mut reader: R,
    result_limit: u64,
    transport_limit: u64,
    overflow: Arc<AtomicBool>,
) -> Result<(Vec<u8>, bool), std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let semantic_limit = result_limit.saturating_add(JSONL_METADATA_SLACK_BYTES);
    let line_limit = semantic_limit;
    let mut retained = Vec::new();
    let mut line = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut raw_bytes = 0_u64;
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        for byte in &chunk[..count] {
            raw_bytes = raw_bytes.saturating_add(1);
            if raw_bytes > transport_limit {
                overflow.store(true, Ordering::Relaxed);
                return Ok((retained, true));
            }
            if *byte == b'\n' {
                if !retain_jsonl_event(&line, &mut retained, semantic_limit) {
                    overflow.store(true, Ordering::Relaxed);
                    return Ok((retained, true));
                }
                line.clear();
            } else if u64::try_from(line.len()).unwrap_or(u64::MAX) < line_limit {
                line.push(*byte);
            } else {
                overflow.store(true, Ordering::Relaxed);
                return Ok((retained, true));
            }
        }
    }
    if !line.is_empty() && !retain_jsonl_event(&line, &mut retained, semantic_limit) {
        overflow.store(true, Ordering::Relaxed);
        return Ok((retained, true));
    }
    Ok((retained, false))
}

pub(crate) fn retain_jsonl_event(line: &[u8], retained: &mut Vec<u8>, limit: u64) -> bool {
    if line.is_empty() {
        return true;
    }
    let bytes = match serde_json::from_slice::<Value>(line) {
        Ok(event) => match event.get("type").and_then(Value::as_str) {
            Some("message_end")
                if event.pointer("/message/role").and_then(Value::as_str) == Some("assistant") =>
            {
                serde_json::to_vec(&json!({
                    "type": "message_end",
                    "message": {
                        "role": "assistant",
                        "provider": event.pointer("/message/provider"),
                        "model": event.pointer("/message/model"),
                        "content": event.pointer("/message/content"),
                    },
                }))
                .expect("JSON values serialize")
            }
            Some("agent_end") => serde_json::to_vec(&json!({
                "type": "agent_end",
                "stopReason": event.get("stopReason"),
            }))
            .expect("JSON values serialize"),
            Some("turn_end")
                if event.pointer("/message/role").and_then(Value::as_str) == Some("assistant") =>
            {
                b"{\"type\":\"turn_end\",\"message\":{\"role\":\"assistant\"}}".to_vec()
            }
            _ => return true,
        },
        Err(_) => line.to_vec(),
    };
    if u64::try_from(retained.len())
        .unwrap_or(u64::MAX)
        .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        .saturating_add(1)
        > limit
    {
        return false;
    }
    retained.extend_from_slice(&bytes);
    retained.push(b'\n');
    true
}

pub(crate) fn join_capture(
    result: Result<Result<(Vec<u8>, bool), std::io::Error>, JoinError>,
) -> Result<(Vec<u8>, bool), RunnerError> {
    result
        .map_err(RunnerError::CaptureTask)?
        .map_err(RunnerError::Io)
}

pub(crate) async fn read_bounded<R>(
    reader: R,
    limit: u64,
) -> Result<(Vec<u8>, bool), std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take(limit + CAPTURE_OVERHEAD_BYTES)
        .read_to_end(&mut bytes)
        .await?;
    let truncated = bytes.len() as u64 > limit;
    if truncated {
        bytes.truncate(usize::try_from(limit).expect("validated output limit fits usize"));
    }
    Ok((bytes, truncated))
}
