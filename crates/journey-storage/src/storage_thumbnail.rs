use std::{fs::File, io::{self, Read, Seek, SeekFrom}, os::unix::fs::FileExt, sync::Arc};
use ffmpeg_next as ffmpeg;
use crate::storage_interface::{StoreError, StoreErrorKind};
use crate::storage_image_reduction::{MAX_DECODED_IMAGE_PIXELS, initialize_ffmpeg, normalize_legacy_jpeg_frame};

const MAX_THUMBNAIL_EDGE: u32 = 640;

pub(crate) fn generate_thumbnail(
    file: Arc<File>,
    metadata_len: u64,
    payload_len: u64,
    time_ms: u64,
) -> Result<Vec<u8>, StoreError> {
    initialize_ffmpeg()?;

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
        let decoded_pixels = u64::from(decoder.width())
            .checked_mul(u64::from(decoder.height()))
            .ok_or_else(|| internal("Video dimensions overflow"))?;
        if decoder.width() == 0 || decoder.height() == 0 || decoded_pixels > MAX_DECODED_IMAGE_PIXELS {
            return Err(internal(format!(
                "Video frame exceeds the {MAX_DECODED_IMAGE_PIXELS}-pixel decoding limit"
            )));
        }
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
                    *first_thumbnail = Some(encode_jpeg(&mut frame)?);
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
                    return Ok(Some(encode_jpeg(&mut frame)?));
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

fn encode_jpeg(frame: &mut ffmpeg::frame::Video) -> Result<Vec<u8>, StoreError> {
    normalize_legacy_jpeg_frame(frame);
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

fn ffmpeg_error(operation: &str, error: ffmpeg::Error) -> StoreError {
    internal(format!("FFmpeg could not {operation}: {error}"))
}

fn internal(detail: impl Into<String>) -> StoreError {
    StoreError::new(StoreErrorKind::Internal, detail)
}
