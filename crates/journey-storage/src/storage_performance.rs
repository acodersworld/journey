use std::{future::poll_fn, sync::Arc};

use bytes::Bytes;
use http::{header, Method, Request, Response, StatusCode};
use journey_performance::{CHUNK_BYTES, DEFAULT_TRANSFER_BYTES, Sample};

use crate::http2_storage_service::{poll_send_capacity, ServiceError, TrackedRespond};

pub(super) async fn handle(
    sample: &Arc<Sample>,
    request: Request<h2::RecvStream>,
    mut respond: TrackedRespond,
) -> Result<(), ServiceError> {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        respond.send_response(Response::builder().status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD").body(())?, true)?;
        return Ok(());
    }
    let bytes = match parse_bytes(request.uri().query()) {
        Ok(bytes) => bytes,
        Err(()) => {
            respond.send_response(Response::builder().status(StatusCode::BAD_REQUEST).body(())?, true)?;
            return Ok(());
        }
    };
    log::info!("storage_performance_preparation bytes={bytes}");
    let prepared = match sample.prepare().await {
        Ok(file) => file,
        Err(error) => {
            log::error!("storage_performance_preparation_failed error={error}");
            respond.send_response(Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).body(())?, true)?;
            return Ok(());
        }
    };
    let mut reader = if request.method() == Method::GET {
        match prepared.reader().await {
            Ok(reader) => Some(reader),
            Err(error) => {
                log::error!("storage_performance_reader_failed error={error}");
                respond.send_response(Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).body(())?, true)?;
                return Ok(());
            }
        }
    } else { None };
    let mut stream = respond.send_response(Response::builder().status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes)
        .header(header::CACHE_CONTROL, "no-store, no-transform")
        .body(())?, request.method() == Method::HEAD)?;
    if request.method() == Method::HEAD {
        return Ok(());
    }
    let mut reader = reader.take().unwrap();
    let mut remaining = bytes;
    let mut buffer = vec![0_u8; CHUNK_BYTES.min(16 * 1024)];
    log::info!("storage_performance_started bytes={bytes}");
    while remaining > 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;
        stream.reserve_capacity(requested);
        let capacity = poll_fn(|cx| poll_send_capacity(&mut stream, cx)).await?;
        let count = reader.read(&mut buffer[..requested.min(capacity)]).await
            .map_err(|error| ServiceError::ReaderFailure(error.to_string()))?;
        remaining -= count as u64;
        stream.send_data(Bytes::copy_from_slice(&buffer[..count]), remaining == 0)?;
    }
    log::info!("storage_performance_completed bytes={bytes}");
    Ok(())
}

fn parse_bytes(query: Option<&str>) -> Result<u64, ()> {
    let Some(query) = query else { return Ok(DEFAULT_TRANSFER_BYTES) };
    let value = query.strip_prefix("bytes=").ok_or(())?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    value.parse::<u64>().ok().filter(|bytes| *bytes > 0).ok_or(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_length_query_is_strict() {
        assert_eq!(parse_bytes(None), Ok(DEFAULT_TRANSFER_BYTES));
        assert_eq!(parse_bytes(Some("bytes=1")), Ok(1));
        for invalid in ["", "bytes=0", "bytes=-1", "bytes=1&other=2", "bytes=1K"] {
            assert!(parse_bytes(Some(invalid)).is_err());
        }
    }
}
