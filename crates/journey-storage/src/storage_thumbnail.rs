use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;

use bytes::Bytes;
use ffmpeg_next as ffmpeg;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::storage_interface::{StoreError, StoreErrorKind};

const MAX_THUMBNAIL_EDGE: u32 = 640;
const MAX_THUMBNAIL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CONCURRENT_DECODES: usize = 2;
static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(1);
static FFMPEG_INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) struct ThumbnailCache {
    directory: Arc<PathBuf>,
    time_ms: u64,
    decode_slots: Arc<Semaphore>,
    jobs: Arc<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>>,
    #[cfg(test)]
    generation_count: Arc<AtomicUsize>,
}

impl ThumbnailCache {
    pub(crate) fn open(directory: impl Into<PathBuf>, time_ms: u64) -> Result<Self, StoreError> {
        let directory = directory.into();
        ensure_cache_directory(&directory)?;
        for entry in fs::read_dir(&directory).map_err(|error| cache_io_error("list thumbnail cache", error))? {
            let entry = entry.map_err(|error| cache_io_error("read thumbnail cache entry", error))?;
            if entry.file_name().to_string_lossy().starts_with(".tmp-") {
                let metadata = entry
                    .file_type()
                    .map_err(|error| cache_io_error("inspect thumbnail cache entry", error))?;
                if metadata.is_file() || metadata.is_symlink() {
                    fs::remove_file(entry.path())
                        .map_err(|error| cache_io_error("remove incomplete thumbnail cache entry", error))?;
                }
            }
        }

        Ok(Self {
            directory: Arc::new(directory),
            time_ms,
            decode_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_DECODES)),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            generation_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(crate) async fn get_or_generate(
        &self,
        file: Arc<File>,
        metadata_len: u64,
        payload_len: u64,
        object_id: String,
    ) -> Result<Bytes, StoreError> {
        let cache_name = self.cache_name(&object_id);
        let job_lock = self.job_lock(&cache_name)?;
        let _job_guard = job_lock.lock().await;
        let cache_path = self.directory.join(&cache_name);
        let read_path = cache_path.clone();
        let cached = tokio::task::spawn_blocking(move || read_cached_thumbnail(&read_path))
            .await
            .map_err(|error| unavailable(format!("Thumbnail cache read task failed: {error}")))??;
        if let Some(cached) = cached {
            return Ok(Bytes::from(cached));
        }

        let permit = Arc::clone(&self.decode_slots)
            .acquire_owned()
            .await
            .map_err(|error| unavailable(format!("Thumbnail decoder limit is unavailable: {error}")))?;
        let directory = Arc::clone(&self.directory);
        let time_ms = self.time_ms;
        #[cfg(test)]
        let generation_count = Arc::clone(&self.generation_count);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            #[cfg(test)]
            generation_count.fetch_add(1, Ordering::Relaxed);
            let jpeg = generate_thumbnail(file, metadata_len, payload_len, time_ms)?;
            if !is_valid_jpeg(&jpeg) {
                return Err(internal("FFmpeg produced an invalid JPEG thumbnail"));
            }
            write_thumbnail_atomically(&directory, &cache_path, &jpeg)?;
            Ok(Bytes::from(jpeg))
        })
        .await
        .map_err(|error| unavailable(format!("Thumbnail generation task failed: {error}")))?
    }

    #[cfg(test)]
    pub(crate) fn generation_count(&self) -> usize {
        self.generation_count.load(Ordering::Relaxed)
    }

    fn cache_name(&self, object_id: &str) -> String {
        let identity = format!(
            "thumbnail-v1:{object_id}:{}ms:{MAX_THUMBNAIL_EDGE}px:mjpeg-quality-2",
            self.time_ms
        );
        format!("{}.jpg", lowercase_hex(&Sha256::digest(identity.as_bytes())))
    }

    fn job_lock(&self, cache_name: &str) -> Result<Arc<AsyncMutex<()>>, StoreError> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| internal("Thumbnail job lock is poisoned"))?;
        jobs.retain(|_, job| job.strong_count() > 0);
        if let Some(job) = jobs.get(cache_name).and_then(Weak::upgrade) {
            return Ok(job);
        }
        let job = Arc::new(AsyncMutex::new(()));
        jobs.insert(cache_name.to_owned(), Arc::downgrade(&job));
        Ok(job)
    }
}

fn ensure_cache_directory(directory: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(unavailable(
            format!("Thumbnail cache path {} is not a real directory", directory.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(directory).map_err(|error| cache_io_error("create thumbnail cache", error))
        }
        Err(error) => Err(cache_io_error("inspect thumbnail cache", error)),
    }
}

fn read_cached_thumbnail(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(cache_io_error("inspect thumbnail cache entry", error)),
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_THUMBNAIL_BYTES {
        return Ok(None);
    }

    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ELOOP) => {
            return Ok(None);
        }
        Err(error) => return Err(cache_io_error("open thumbnail cache entry", error)),
    };
    let actual_metadata = file
        .metadata()
        .map_err(|error| cache_io_error("read thumbnail cache metadata", error))?;
    if !actual_metadata.is_file()
        || actual_metadata.len() == 0
        || actual_metadata.len() > MAX_THUMBNAIL_BYTES
    {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(actual_metadata.len() as usize);
    file.take(MAX_THUMBNAIL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| cache_io_error("read thumbnail cache entry", error))?;
    if bytes.len() as u64 != actual_metadata.len() || !is_valid_jpeg(&bytes) {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn write_thumbnail_atomically(directory: &Path, cache_path: &Path, jpeg: &[u8]) -> Result<(), StoreError> {
    if jpeg.is_empty() || jpeg.len() as u64 > MAX_THUMBNAIL_BYTES {
        return Err(internal("Generated JPEG thumbnail has an invalid size"));
    }

    let mut temporary = None;
    let mut file = None;
    for _ in 0..32 {
        let id = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".tmp-{}-{id}", std::process::id()));
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        match options.open(&path) {
            Ok(created) => {
                temporary = Some(path);
                file = Some(created);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(cache_io_error("create temporary thumbnail", error)),
        }
    }
    let temporary = temporary.ok_or_else(|| unavailable("Could not allocate a temporary thumbnail file"))?;
    let mut file = file.unwrap();
    let write_result = file.write_all(jpeg).and_then(|_| file.sync_all());
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(cache_io_error("write temporary thumbnail", error));
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, cache_path) {
        let _ = fs::remove_file(&temporary);
        return Err(cache_io_error("publish thumbnail cache entry", error));
    }
    Ok(())
}

fn is_valid_jpeg(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() as u64 > MAX_THUMBNAIL_BYTES {
        return false;
    }
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    if decoder.read_info().is_err() {
        return false;
    }
    let Some(info) = decoder.info() else {
        return false;
    };
    if info.width == 0
        || info.height == 0
        || u32::from(info.width) > MAX_THUMBNAIL_EDGE
        || u32::from(info.height) > MAX_THUMBNAIL_EDGE
    {
        return false;
    }
    decoder.decode().is_ok()
}

fn generate_thumbnail(
    file: Arc<File>,
    metadata_len: u64,
    payload_len: u64,
    time_ms: u64,
) -> Result<Vec<u8>, StoreError> {
    FFMPEG_INITIALIZED
        .get_or_init(|| ffmpeg::init().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| internal(format!("Could not initialize FFmpeg: {error}")))?;

    let payload_reader = PayloadReader::new(file, metadata_len, payload_len);
    let stream_io = ffmpeg::format::context::StreamIo::from_read_seek(payload_reader)
        .map_err(|error| ffmpeg_error("create video input", error))?;
    let mut input = ffmpeg::format::input_from_stream(stream_io, None, None)
        .map_err(|error| ffmpeg_error("open video input", error))?;
    let duration = input.duration();
    let target_ms = if duration > 0 && (time_ms as i128) * 1_000 > i128::from(duration) {
        0
    } else {
        time_ms
    };
    let (video_stream_index, time_base, stream_start_time, mut decoder) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| internal("Video container has no decodable video stream"))?;
        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|error| ffmpeg_error("read video stream parameters", error))?;
        let decoder = context
            .decoder()
            .video()
            .map_err(|error| ffmpeg_error("open video decoder", error))?;
        (stream.index(), stream.time_base(), stream.start_time().max(0), decoder)
    };

    let mut first_thumbnail = None;
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => continue,
            Err(error) => return Err(ffmpeg_error("read video packet", error)),
        }
        if packet.stream() != video_stream_index {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|error| ffmpeg_error("decode video packet", error))?;
        if let Some(thumbnail) = receive_thumbnail_frame(
            &mut decoder,
            time_base,
            stream_start_time,
            target_ms,
            &mut first_thumbnail,
        )? {
            return Ok(thumbnail);
        }
    }
    decoder
        .send_eof()
        .map_err(|error| ffmpeg_error("finish video decoding", error))?;
    if let Some(thumbnail) = receive_thumbnail_frame(
        &mut decoder,
        time_base,
        stream_start_time,
        target_ms,
        &mut first_thumbnail,
    )? {
        return Ok(thumbnail);
    }
    first_thumbnail.ok_or_else(|| internal("Video did not contain a decodable frame"))
}

fn receive_thumbnail_frame(
    decoder: &mut ffmpeg::decoder::Video,
    time_base: ffmpeg::Rational,
    stream_start_time: i64,
    target_ms: u64,
    first_thumbnail: &mut Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>, StoreError> {
    loop {
        let mut frame = ffmpeg::frame::Video::empty();
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                if frame.is_corrupt() || frame.has_decode_errors() {
                    return Err(internal("FFmpeg decoded a corrupt video frame"));
                }
                if first_thumbnail.is_none() {
                    *first_thumbnail = Some(encode_jpeg(&frame)?);
                }
                if target_ms == 0 {
                    return Ok(first_thumbnail.take());
                }
                if frame
                    .timestamp()
                    .map(|timestamp| timestamp.saturating_sub(stream_start_time))
                    .and_then(|timestamp| timestamp_to_ms(timestamp, time_base))
                    .is_some_and(|frame_ms| frame_ms >= target_ms)
                {
                    return Ok(Some(encode_jpeg(&frame)?));
                }
            }
            Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => return Ok(None),
            Err(ffmpeg::Error::Eof) => return Ok(None),
            Err(error) => return Err(ffmpeg_error("decode video frame", error)),
        }
    }
}

fn timestamp_to_ms(timestamp: i64, time_base: ffmpeg::Rational) -> Option<u64> {
    if timestamp < 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
        return None;
    }
    let milliseconds = i128::from(timestamp)
        .checked_mul(i128::from(time_base.numerator()))?
        .checked_mul(1_000)?
        / i128::from(time_base.denominator());
    u64::try_from(milliseconds).ok()
}

fn encode_jpeg(frame: &ffmpeg::frame::Video) -> Result<Vec<u8>, StoreError> {
    let width = frame.width();
    let height = frame.height();
    if width == 0 || height == 0 {
        return Err(internal("Decoded video frame has empty dimensions"));
    }
    let scale = (MAX_THUMBNAIL_EDGE as f64 / width.max(height) as f64).min(1.0);
    let output_width = ((width as f64 * scale).round() as u32).max(1);
    let output_height = ((height as f64 * scale).round() as u32).max(1);
    let output_format = ffmpeg::format::Pixel::YUV444P;
    let mut scaler = ffmpeg::software::scaling::Context::get(
        frame.format(),
        width,
        height,
        output_format,
        output_width,
        output_height,
        ffmpeg::software::scaling::flag::Flags::BILINEAR,
    )
    .map_err(|error| ffmpeg_error("prepare thumbnail scaling", error))?;
    let coefficients = unsafe { ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_DEFAULT) };
    if coefficients.is_null() {
        return Err(internal("Could not configure thumbnail color conversion"));
    }
    let input_full_range = if frame.color_range() == ffmpeg::color::Range::JPEG {
        1
    } else {
        0
    };
    let range_result = unsafe {
        ffmpeg::ffi::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            coefficients,
            input_full_range,
            coefficients,
            1,
            0,
            1 << 16,
            1 << 16,
        )
    };
    if range_result < 0 {
        return Err(internal("Could not configure full-range thumbnail scaling"));
    }
    let mut scaled = ffmpeg::frame::Video::empty();
    scaler
        .run(frame, &mut scaled)
        .map_err(|error| ffmpeg_error("scale video thumbnail", error))?;
    scaled.set_color_range(ffmpeg::color::Range::JPEG);

    let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::MJPEG)
        .ok_or_else(|| internal("FFmpeg MJPEG encoder is unavailable"))?;
    let mut video_encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|error| ffmpeg_error("create JPEG encoder", error))?;
    video_encoder.set_width(output_width);
    video_encoder.set_height(output_height);
    video_encoder.set_format(output_format);
    video_encoder.set_color_range(ffmpeg::color::Range::JPEG);
    video_encoder.set_time_base((1, 25));
    video_encoder.set_quality(2);
    let mut encoder = video_encoder
        .open_as(codec)
        .map_err(|error| ffmpeg_error("open JPEG encoder", error))?;
    encoder
        .send_frame(&scaled)
        .map_err(|error| ffmpeg_error("encode JPEG thumbnail", error))?;

    let mut jpeg = Vec::new();
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {
                if let Some(data) = packet.data() {
                    jpeg.extend_from_slice(data);
                }
            }
            Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
            Err(ffmpeg::Error::Eof) => break,
            Err(error) => return Err(ffmpeg_error("read encoded JPEG thumbnail", error)),
        }
    }
    if jpeg.is_empty() {
        return Err(internal("FFmpeg did not produce a JPEG thumbnail"));
    }
    Ok(jpeg)
}

struct PayloadReader {
    file: Arc<File>,
    metadata_len: u64,
    payload_len: u64,
    position: u64,
}

impl PayloadReader {
    fn new(file: Arc<File>, metadata_len: u64, payload_len: u64) -> Self {
        Self {
            file,
            metadata_len,
            payload_len,
            position: 0,
        }
    }
}

impl Read for PayloadReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.position >= self.payload_len {
            return Ok(0);
        }
        let available = self.payload_len - self.position;
        let read_len = buffer
            .len()
            .min(usize::try_from(available).unwrap_or(usize::MAX));
        let physical_offset = self
            .metadata_len
            .checked_add(self.position)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload offset overflow"))?;
        loop {
            match self.file.read_at(&mut buffer[..read_len], physical_offset) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short object payload")),
                Ok(count) => {
                    self.position = self.position.checked_add(count as u64).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "payload cursor overflow")
                    })?;
                    return Ok(count);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Seek for PayloadReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next_position = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.payload_len) + i128::from(offset),
        };
        if !(0..=i128::from(self.payload_len)).contains(&next_position) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek is outside object payload"));
        }
        self.position = u64::try_from(next_position)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "payload seek overflow"))?;
        Ok(self.position)
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn ffmpeg_error(operation: &str, error: ffmpeg::Error) -> StoreError {
    internal(format!("FFmpeg could not {operation}: {error}"))
}

fn cache_io_error(operation: &str, error: io::Error) -> StoreError {
    unavailable(format!("Could not {operation}: {error}"))
}

fn internal(detail: impl Into<String>) -> StoreError {
    StoreError::new(StoreErrorKind::Internal, detail)
}

fn unavailable(detail: impl Into<String>) -> StoreError {
    StoreError::new(StoreErrorKind::Unavailable, detail)
}
