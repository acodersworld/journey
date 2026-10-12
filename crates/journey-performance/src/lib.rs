//! Temporary sample files and transfer-size parsing for opt-in diagnostics.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{Arc, atomic::{AtomicU64, Ordering}},
};
use tokio::{io::{AsyncReadExt, AsyncSeekExt}, sync::OnceCell};

pub const SAMPLE_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_TRANSFER_BYTES: u64 = 256 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 64 * 1024;
static NEXT_FILE: AtomicU64 = AtomicU64::new(1);

pub fn parse_size(value: &str) -> Result<u64, String> {
    let digit_end = value.bytes().take_while(u8::is_ascii_digit).count();
    let (digits, suffix) = value.split_at(digit_end);
    if digits.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err("size must be a positive integer with a supported suffix".to_owned());
    }
    let multiplier = match suffix.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KIB" => 1024,
        "KB" => 1000,
        "M" | "MIB" => 1024_u64.pow(2),
        "MB" => 1000_u64.pow(2),
        "G" | "GIB" => 1024_u64.pow(3),
        "GB" => 1000_u64.pow(3),
        _ => return Err("unsupported size suffix".to_owned()),
    };
    digits.parse::<u64>().ok().and_then(|number| number.checked_mul(multiplier))
        .filter(|number| *number > 0)
        .ok_or_else(|| "size must fit in a positive unsigned 64-bit integer".to_owned())
}

#[derive(Debug, Default)]
pub struct Sample {
    ready: OnceCell<Arc<SampleFile>>,
}

impl Sample {
    pub async fn prepare(&self) -> io::Result<Arc<SampleFile>> {
        self.ready.get_or_try_init(|| async {
            let file = tokio::task::spawn_blocking(create_file).await.map_err(io::Error::other)??;
            Ok(Arc::new(file))
        }).await.map(Arc::clone)
    }
}

#[derive(Debug)]
pub struct SampleFile {
    path: PathBuf,
}

impl SampleFile {
    pub async fn reader(&self) -> io::Result<SampleReader> {
        Ok(SampleReader { file: tokio::fs::File::open(&self.path).await?, offset: 0 })
    }
}

impl Drop for SampleFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn create_file() -> io::Result<SampleFile> {
    let mut source = File::open("/dev/urandom")?;
    let (path, mut output) = loop {
        let path = std::env::temp_dir().join(format!(
            "journey-performance-{}-{}", std::process::id(), NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        let mut buffer = [0_u8; CHUNK_BYTES];
        for _ in 0..(SAMPLE_BYTES / CHUNK_BYTES as u64) {
            source.read_exact(&mut buffer)?;
            output.write_all(&buffer)?;
        }
        output.flush()
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(SampleFile { path })
}

pub struct SampleReader {
    file: tokio::fs::File,
    offset: u64,
}

impl SampleReader {
    pub async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.offset == SAMPLE_BYTES {
            self.file.seek(std::io::SeekFrom::Start(0)).await?;
            self.offset = 0;
        }
        let limit = buffer.len().min((SAMPLE_BYTES - self.offset) as usize);
        let count = self.file.read(&mut buffer[..limit]).await?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sample file ended early"));
        }
        self.offset += count as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_suffixes_and_overflow() {
        assert_eq!(parse_size("512K"), Ok(524288));
        assert_eq!(parse_size("10MB"), Ok(10_000_000));
        assert_eq!(parse_size("256MiB"), Ok(DEFAULT_TRANSFER_BYTES));
        assert_eq!(parse_size("3gib"), Ok(3 * 1024_u64.pow(3)));
        assert_eq!(parse_size("99"), Ok(99));
        for invalid in ["0", "1.5M", "1T", "18446744073709551616", "18446744073709551615G"] {
            assert!(parse_size(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn concurrent_preparation_and_repeated_reads() {
        let sample = Sample::default();
        let (first, second) = tokio::join!(sample.prepare(), sample.prepare());
        let first = first.unwrap();
        assert!(Arc::ptr_eq(&first, &second.unwrap()));
        let path = first.path.clone();
        assert!(path.exists());
        let (reader_a, reader_b) = tokio::join!(first.reader(), first.reader());
        let (mut reader_a, mut reader_b) = (reader_a.unwrap(), reader_b.unwrap());
        let mut a = [0; 64];
        let mut b = [0; 64];
        let (count_a, count_b) = tokio::join!(reader_a.read(&mut a), reader_b.read(&mut b));
        assert_eq!(count_a.unwrap(), 64);
        assert_eq!(count_b.unwrap(), 64);
        assert_eq!(a, b);
        let mut reader = first.reader().await.unwrap();
        let mut buffer = [0; 17];
        let mut remaining = SAMPLE_BYTES + 17;
        while remaining > 0 {
            let count = reader.read(&mut buffer[..remaining.min(17) as usize]).await.unwrap();
            remaining -= count as u64;
        }
        drop(reader);
        drop(reader_a);
        drop(reader_b);
        drop(first);
        drop(sample);
        assert!(!path.exists());
    }
}
