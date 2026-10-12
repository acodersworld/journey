use std::time::Instant;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::storage::StorageClient;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Request {
    pub bytes: Option<u64>,
    pub key: Option<String>,
    pub concurrency: usize,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Transfer {
    pub bytes_received: u64,
    pub first_body_seconds: Option<f64>,
    pub total_seconds: f64,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Result {
    pub source: String,
    pub transport: String,
    pub requested_concurrency: usize,
    pub completed_transfers: usize,
    pub bytes_received: u64,
    pub duration_seconds: f64,
    pub aggregate_mib_per_second: f64,
    pub failures: usize,
    pub initial_stream_window_size: u32,
    pub initial_connection_window_size: u32,
    pub transfers: Vec<Transfer>,
}

pub async fn run<S: StorageClient>(
    storage: S,
    request: Request,
    transport: String,
    initial_stream_window_size: u32,
    initial_connection_window_size: u32,
) -> std::result::Result<Result, String> {
    if request.concurrency == 0 {
        return Err("concurrency must be a positive integer".to_owned());
    }
    if request.bytes.is_some() == request.key.is_some() {
        return Err("choose either bytes or key".to_owned());
    }
    if request.bytes == Some(0) || request.key.as_deref() == Some("") {
        return Err("bytes and key must be nonempty".to_owned());
    }
    let source = if let Some(bytes) = request.bytes {
        log::info!("site_performance_preparation source=storage_sample bytes={bytes}");
        let response = storage.performance(bytes, true).await?;
        if response.status != http::StatusCode::OK {
            return Err(format!("storage sample preparation failed with HTTP {}", response.status));
        }
        "storage_sample_file".to_owned()
    } else {
        format!("object:{}", request.key.as_deref().unwrap())
    };
    log::info!("site_performance_started source={source:?} concurrency={}", request.concurrency);
    let started = Instant::now();
    let mut tasks = JoinSet::new();
    for _ in 0..request.concurrency {
        let storage = storage.clone();
        let key = request.key.clone();
        let bytes = request.bytes;
        tasks.spawn(async move { transfer(storage, key, bytes).await });
    }
    let mut transfers = Vec::with_capacity(request.concurrency);
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(transfer) => transfers.push(transfer),
            Err(error) => transfers.push(Transfer {
                bytes_received: 0, first_body_seconds: None,
                total_seconds: started.elapsed().as_secs_f64(), error: Some(error.to_string()),
            }),
        }
    }
    let duration_seconds = started.elapsed().as_secs_f64();
    let bytes_received = transfers.iter().map(|transfer| transfer.bytes_received).sum::<u64>();
    let failures = transfers.iter().filter(|transfer| transfer.error.is_some()).count();
    log::info!("site_performance_completed source={source:?} bytes={bytes_received} failures={failures} duration_seconds={duration_seconds:.3}");
    Ok(Result {
        source, transport, requested_concurrency: request.concurrency,
        completed_transfers: request.concurrency - failures, bytes_received, duration_seconds,
        aggregate_mib_per_second: bytes_received as f64 / duration_seconds / 1_048_576.0,
        failures, initial_stream_window_size, initial_connection_window_size, transfers,
    })
}

async fn transfer<S: StorageClient>(storage: S, key: Option<String>, bytes: Option<u64>) -> Transfer {
    let started = Instant::now();
    let response = match (key.as_deref(), bytes) {
        (Some(key), None) => storage.get(key, None, false, false).await,
        (None, Some(bytes)) => storage.performance(bytes, false).await,
        _ => unreachable!(),
    };
    let mut received = 0_u64;
    let mut first_body_seconds = None;
    let mut error = None;
    match response {
        Err(reason) => error = Some(reason),
        Ok(response) if response.status != http::StatusCode::OK => {
            error = Some(format!("storage returned HTTP {}", response.status));
        }
        Ok(mut response) => {
            let declared_length = response.headers.get(http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            while let Some(chunk) = response.body.next().await {
                match chunk {
                    Ok(chunk) => {
                        if first_body_seconds.is_none() && !chunk.is_empty() {
                            first_body_seconds = Some(started.elapsed().as_secs_f64());
                        }
                        received = received.saturating_add(chunk.len() as u64);
                    }
                    Err(reason) => { error = Some(reason.to_string()); break; }
                }
            }
            if error.is_none() && bytes.is_some_and(|expected| received != expected) {
                error = Some(format!("received {received} bytes; expected {}", bytes.unwrap()));
            } else if error.is_none() && declared_length.is_some_and(|expected| received != expected) {
                error = Some(format!("received {received} bytes; Content-Length was {}", declared_length.unwrap()));
            }
        }
    }
    Transfer { bytes_received: received, first_body_seconds,
        total_seconds: started.elapsed().as_secs_f64(), error }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::{Arc, atomic::{AtomicUsize, Ordering}}};
    use bytes::Bytes;
    use futures_util::stream;

    use super::*;
    use crate::storage::{StorageBody, StorageResponse};

    #[derive(Clone)]
    struct MockStorage {
        calls: Arc<AtomicUsize>,
        unavailable: bool,
    }

    impl MockStorage {
        fn response(&self, head: bool, length: u64) -> std::result::Result<StorageResponse, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.unavailable { return Err("storage unavailable".to_owned()); }
            let mut headers = http::HeaderMap::new();
            headers.insert(http::header::CONTENT_LENGTH, length.to_string().parse().unwrap());
            let body: StorageBody = if head { Box::pin(stream::empty()) } else {
                Box::pin(stream::once(async move { Ok(Bytes::from(vec![1; length as usize])) }))
            };
            Ok(StorageResponse { status: http::StatusCode::OK, headers, body })
        }
    }

    impl StorageClient for MockStorage {
        async fn put_file(&self, _: &str, _: &Path) -> std::result::Result<String, String> { unreachable!() }
        async fn put_stream(&self, _: &str, _: Option<u64>, _: StorageBody, _: u64) -> std::result::Result<(String, u64), String> { unreachable!() }
        async fn get(&self, _: &str, _: Option<&str>, _: bool, _: bool) -> std::result::Result<StorageResponse, String> { self.response(false, 5) }
        async fn performance(&self, bytes: u64, head: bool) -> std::result::Result<StorageResponse, String> { self.response(head, bytes) }
    }

    #[tokio::test]
    async fn benchmark_prepares_once_and_drains_concurrent_gets() {
        let calls = Arc::new(AtomicUsize::new(0));
        let storage = MockStorage { calls: Arc::clone(&calls), unavailable: false };
        let result = run(storage, Request { bytes: Some(3), key: None, concurrency: 4 },
            "websocket".to_owned(), 512, 1024).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 5);
        assert_eq!(result.completed_transfers, 4);
        assert_eq!(result.bytes_received, 12);
        assert_eq!(result.failures, 0);
        assert!(result.transfers.iter().all(|transfer| transfer.first_body_seconds.is_some()));
    }

    #[tokio::test]
    async fn unavailable_storage_fails_preparation_without_transfers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let storage = MockStorage { calls: Arc::clone(&calls), unavailable: true };
        assert!(run(storage, Request { bytes: Some(3), key: None, concurrency: 4 },
            "h2c".to_owned(), 512, 1024).await.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn object_mode_uses_normal_get_without_preparation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let storage = MockStorage { calls: Arc::clone(&calls), unavailable: false };
        let result = run(storage, Request { bytes: None, key: Some("media/key".to_owned()), concurrency: 2 },
            "h2c".to_owned(), 512, 1024).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(result.bytes_received, 10);
    }
}

pub fn print(result: &Result, json: bool) -> std::result::Result<(), serde_json::Error> {
    if json {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        if result.source == "storage_sample_file" {
            println!("Source: {} (repeated reads may use OS page cache)", result.source);
        } else {
            println!("Source: {}", result.source);
        }
        println!("Transport: {} | window: {} stream, {} connection", result.transport,
            result.initial_stream_window_size, result.initial_connection_window_size);
        println!("Transfers: {}/{} completed, {} failures", result.completed_transfers,
            result.requested_concurrency, result.failures);
        println!("Received: {} bytes in {:.3} s | {:.2} MiB/s aggregate", result.bytes_received,
            result.duration_seconds, result.aggregate_mib_per_second);
        for (index, transfer) in result.transfers.iter().enumerate() {
            println!("  {}: {} bytes, first body {:?} s, total {:.3} s{}", index + 1,
                transfer.bytes_received, transfer.first_body_seconds, transfer.total_seconds,
                transfer.error.as_ref().map(|error| format!(", error: {error}")).unwrap_or_default());
        }
    }
    Ok(())
}
