use std::{
    io::Cursor,
    sync::{Arc, OnceLock},
};

use bytes::Bytes;
use ffmpeg_next as ffmpeg;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::storage_interface::{
    ImageFit, ImageOutputFormat, ImageQuality, ImageReductionRequest, ImageReductionSize, ReducedImage,
    StoreError, StoreErrorKind,
};

pub(crate) const MAX_IMAGE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_DECODED_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_JPEG_QUALITY_ATTEMPTS: usize = 10;
const JPEG_QUALITY_LEVELS: [usize; MAX_JPEG_QUALITY_ATTEMPTS] = [
    2, 5, 8, 11, 14, 17, 20, 24, 28, 31,
];
static DECODE_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
static FFMPEG_INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

pub(crate) fn initialize_ffmpeg() -> Result<(), StoreError> {
    FFMPEG_INITIALIZED
        .get_or_init(|| ffmpeg::init().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| internal(format!("Could not initialize FFmpeg: {error}")))
        .map(|_| ())
}

pub(crate) async fn acquire_image_reduction_slot() -> Result<OwnedSemaphorePermit, StoreError> {
    Arc::clone(DECODE_SLOTS.get_or_init(|| Arc::new(Semaphore::new(2))))
        .acquire_owned()
        .await
        .map_err(|error| StoreError::new(StoreErrorKind::Unavailable, format!("Image decoder limit is unavailable: {error}")))
}

pub(crate) async fn reduce_image(
    bytes: Bytes,
    source_content_type: String,
    request: ImageReductionRequest,
) -> Result<ReducedImage, StoreError> {
    let permit = acquire_image_reduction_slot().await?;
    reduce_image_with_slot(bytes, source_content_type, request, permit).await
}

pub(crate) async fn reduce_image_with_slot(
    bytes: Bytes,
    source_content_type: String,
    request: ImageReductionRequest,
    permit: OwnedSemaphorePermit,
) -> Result<ReducedImage, StoreError> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        process_image_blocking(bytes, &source_content_type, Some(&request), None)
    })
    .await
    .map_err(|error| StoreError::new(StoreErrorKind::Unavailable, format!("Image reduction task failed: {error}")))?
}

pub(crate) async fn reencode_image_with_slot(
    bytes: Bytes,
    source_content_type: String,
    quality: ImageQuality,
    permit: OwnedSemaphorePermit,
) -> Result<ReducedImage, StoreError> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        process_image_blocking(bytes, &source_content_type, None, Some(quality))
    })
    .await
    .map_err(|error| StoreError::new(StoreErrorKind::Unavailable, format!("Image re-encoding task failed: {error}")))?
}

fn process_image_blocking(
    bytes: Bytes,
    source_content_type: &str,
    request: Option<&ImageReductionRequest>,
    quality: Option<ImageQuality>,
) -> Result<ReducedImage, StoreError> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_SOURCE_BYTES {
        return Err(StoreError::new(StoreErrorKind::Capacity, "Source image is empty or exceeds the decoding limit"));
    }
    initialize_ffmpeg()?;

    let orientation = exif_orientation(&bytes).unwrap_or(1);
    let stream_io = ffmpeg::format::context::StreamIo::from_read_seek(Cursor::new(bytes))
        .map_err(|error| ffmpeg_error("create image input", error))?;
    let mut input = ffmpeg::format::input_from_stream(stream_io, None, None)
        .map_err(|error| unsupported(format!("Could not decode image input: {error}")))?;
    let (stream_index, mut decoder) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| unsupported("Image input has no decodable still-image stream"))?;
        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|error| ffmpeg_error("read image stream parameters", error))?;
        let decoder = context
            .decoder()
            .video()
            .map_err(|error| unsupported(format!("Could not open image decoder: {error}")))?;
        let pixels = u64::from(decoder.width())
            .checked_mul(u64::from(decoder.height()))
            .ok_or_else(|| capacity("Image dimensions overflow"))?;
        if decoder.width() == 0 || decoder.height() == 0 || pixels > MAX_DECODED_IMAGE_PIXELS {
            return Err(capacity(format!(
                "Image dimensions exceed the {MAX_DECODED_IMAGE_PIXELS}-pixel decoding limit"
            )));
        }
        (stream.index(), decoder)
    };

    let mut decoded_frame = None;
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => continue,
            Err(error) => return Err(ffmpeg_error("read image packet", error)),
        }
        if packet.stream() != stream_index {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|error| ffmpeg_error("decode image packet", error))?;
        if let Some(frame) = receive_image_frame(&mut decoder)? {
            decoded_frame = Some(frame);
            break;
        }
    }
    if decoded_frame.is_none() {
        decoder
            .send_eof()
            .map_err(|error| ffmpeg_error("finish image decoding", error))?;
        decoded_frame = receive_image_frame(&mut decoder)?;
    }
    let mut frame = decoded_frame.ok_or_else(|| unsupported("Image contains no decodable still frame"))?;
    let (pixels, width, height) = frame_to_rgba(&mut frame)?;
    let (pixels, width, height) = orient_rgba(pixels, width, height, orientation)?;
    let (output_width, output_height, canvas_width, canvas_height) =
        match request {
            Some(request) => output_dimensions(width, height, request.size(), request.fit())?,
            None => (width, height, width, height),
        };

    let mut scaled = ffmpeg::frame::Video::empty();
    let mut scaler = ffmpeg::software::scaling::Context::get(
        ffmpeg::format::Pixel::RGBA,
        width,
        height,
        ffmpeg::format::Pixel::RGBA,
        output_width,
        output_height,
        ffmpeg::software::scaling::flag::Flags::BILINEAR,
    )
    .map_err(|error| ffmpeg_error("prepare image scaling", error))?;
    let mut oriented = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::RGBA, width, height);
    write_rgba_rows(&mut oriented, &pixels, width, height)?;
    scaler
        .run(&oriented, &mut scaled)
        .map_err(|error| ffmpeg_error("scale reduced image", error))?;

    let source_format = reduced_source_format(source_content_type);
    let output_format = if quality.is_some() {
        ReducedFormat::Jpeg
    } else {
        match request.and_then(ImageReductionRequest::format) {
            Some(ImageOutputFormat::Jpeg) => ReducedFormat::Jpeg,
            None => source_format,
        }
    };
    let output_format = if encoder_id(output_format).is_some_and(|id| ffmpeg::encoder::find(id).is_some()) {
        output_format
    } else {
        ReducedFormat::Jpeg
    };
    let is_jpeg = output_format == ReducedFormat::Jpeg;
    let canvas_length = usize::try_from(canvas_width)
        .ok()
        .and_then(|width| usize::try_from(canvas_height).ok().and_then(|height| width.checked_mul(height)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| capacity("Reduced image canvas is too large"))?;
    let mut canvas = vec![255_u8; canvas_length];
    if !is_jpeg {
        for pixel in canvas.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
    let offset_x = (canvas_width - output_width) / 2;
    let offset_y = (canvas_height - output_height) / 2;
    let scaled_stride = scaled.stride(0);
    let scaled_data = scaled.data(0);
    for row in 0..output_height as usize {
        let source_start = row * scaled_stride;
        let destination_start = ((offset_y as usize + row) * canvas_width as usize + offset_x as usize) * 4;
        let row_length = output_width as usize * 4;
        let source = &scaled_data[source_start..source_start + row_length];
        let destination = &mut canvas[destination_start..destination_start + row_length];
        destination.copy_from_slice(source);
        if is_jpeg {
            for pixel in destination.as_chunks_mut::<4>().0 {
                let alpha = u16::from(pixel[3]);
                for channel in &mut pixel[..3] {
                    *channel = ((u16::from(*channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
                }
            }
        }
    }

    let pixel_format = if is_jpeg { ffmpeg::format::Pixel::RGB24 } else { ffmpeg::format::Pixel::RGBA };
    let bytes_per_pixel = if is_jpeg { 3 } else { 4 };
    let output_bytes_len = usize::try_from(canvas_width)
        .ok()
        .and_then(|width| usize::try_from(canvas_height).ok().and_then(|height| width.checked_mul(height)))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .ok_or_else(|| capacity("Reduced image output is too large"))?;
    let mut output_bytes = Vec::with_capacity(output_bytes_len);
    if is_jpeg {
        for pixel in canvas.as_chunks_mut::<4>().0 {
            output_bytes.extend_from_slice(&pixel[..3]);
        }
    } else {
        output_bytes = canvas;
    }
    let mut output_frame = ffmpeg::frame::Video::new(pixel_format, canvas_width, canvas_height);
    write_packed_rows(&mut output_frame, &output_bytes, canvas_width, canvas_height, bytes_per_pixel)?;

    if !is_jpeg {
        if let Ok(encoded) = encode_image(&output_frame, output_format, JPEG_QUALITY_LEVELS[0], false) {
            return Ok(ReducedImage::new(Bytes::from(encoded), output_format.content_type()));
        }
        let jpeg = encode_image(&output_frame, ReducedFormat::Jpeg, JPEG_QUALITY_LEVELS[0], false)?;
        return Ok(ReducedImage::new(Bytes::from(jpeg), ReducedFormat::Jpeg.content_type()));
    }

    let quality_attempt = quality.map(ImageQuality::quantizer);
    let attempts: &[usize] = match quality_attempt.as_ref() {
        Some(value) => std::slice::from_ref(value),
        None if request.is_some_and(|request| request.max_bytes().is_some()) => &JPEG_QUALITY_LEVELS,
        None => &JPEG_QUALITY_LEVELS[..1],
    };
    let max_bytes = request.and_then(ImageReductionRequest::max_bytes);
    for attempt in attempts {
        let output = encode_image(&output_frame, ReducedFormat::Jpeg, *attempt, quality_attempt.is_some())?;
        if max_bytes.is_none_or(|limit| output.len() as u64 <= limit) {
            return Ok(ReducedImage::new(Bytes::from(output), ReducedFormat::Jpeg.content_type()));
        }
    }
    Err(StoreError::new(
        StoreErrorKind::Capacity,
        format!("Reduced JPEG cannot fit within the requested {}-byte limit", max_bytes.unwrap_or(0)),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReducedFormat {
    Jpeg,
    Png,
    WebP,
}

impl ReducedFormat {
    fn content_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::WebP => "image/webp",
        }
    }
}

fn reduced_source_format(content_type: &str) -> ReducedFormat {
    match content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "image/png" => ReducedFormat::Png,
        "image/webp" => ReducedFormat::WebP,
        _ => ReducedFormat::Jpeg,
    }
}

fn encoder_id(format: ReducedFormat) -> Option<ffmpeg::codec::Id> {
    Some(match format {
        ReducedFormat::Jpeg => ffmpeg::codec::Id::MJPEG,
        ReducedFormat::Png => ffmpeg::codec::Id::PNG,
        ReducedFormat::WebP => ffmpeg::codec::Id::WEBP,
    })
}

fn receive_image_frame(
    decoder: &mut ffmpeg::decoder::Video,
) -> Result<Option<ffmpeg::frame::Video>, StoreError> {
    let mut frame = ffmpeg::frame::Video::empty();
    match decoder.receive_frame(&mut frame) {
        Ok(()) => {
            if frame.is_corrupt() || frame.has_decode_errors() {
                return Err(StoreError::new(StoreErrorKind::Corrupt, "FFmpeg decoded a corrupt image frame"));
            }
            let pixels = u64::from(frame.width())
                .checked_mul(u64::from(frame.height()))
                .ok_or_else(|| capacity("Decoded image dimensions overflow"))?;
            if frame.width() == 0 || frame.height() == 0 || pixels > MAX_DECODED_IMAGE_PIXELS {
                return Err(capacity(format!(
                    "Decoded image dimensions exceed the {MAX_DECODED_IMAGE_PIXELS}-pixel limit"
                )));
            }
            Ok(Some(frame))
        }
        Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => Ok(None),
        Err(ffmpeg::Error::Eof) => Ok(None),
        Err(error) => Err(ffmpeg_error("decode image frame", error)),
    }
}

pub(crate) fn normalize_legacy_jpeg_frame(frame: &mut ffmpeg::frame::Video) {
    let format = match frame.format() {
        ffmpeg::format::Pixel::YUVJ420P => ffmpeg::format::Pixel::YUV420P,
        ffmpeg::format::Pixel::YUVJ422P => ffmpeg::format::Pixel::YUV422P,
        ffmpeg::format::Pixel::YUVJ444P => ffmpeg::format::Pixel::YUV444P,
        ffmpeg::format::Pixel::YUVJ440P => ffmpeg::format::Pixel::YUV440P,
        ffmpeg::format::Pixel::YUVJ411P => ffmpeg::format::Pixel::YUV411P,
        _ => return,
    };
    frame.set_format(format);
    frame.set_color_range(ffmpeg::color::Range::JPEG);
}

fn frame_to_rgba(frame: &mut ffmpeg::frame::Video) -> Result<(Vec<u8>, u32, u32), StoreError> {
    normalize_legacy_jpeg_frame(frame);
    let width = frame.width();
    let height = frame.height();
    let mut rgba = ffmpeg::frame::Video::empty();
    let mut scaler = ffmpeg::software::scaling::Context::get(
        frame.format(),
        width,
        height,
        ffmpeg::format::Pixel::RGBA,
        width,
        height,
        ffmpeg::software::scaling::flag::Flags::BILINEAR,
    )
    .map_err(|error| ffmpeg_error("prepare image color conversion", error))?;
    if frame.color_range() == ffmpeg::color::Range::JPEG {
        let coefficients = unsafe { ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_DEFAULT) };
        if coefficients.is_null() || unsafe {
            ffmpeg::ffi::sws_setColorspaceDetails(
                scaler.as_mut_ptr(), coefficients, 1, coefficients, 1, 0, 1 << 16, 1 << 16,
            )
        } < 0 {
            return Err(internal("Could not configure full-range image color conversion"));
        }
    }
    scaler
        .run(frame, &mut rgba)
        .map_err(|error| ffmpeg_error("convert decoded image to RGBA", error))?;
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    let stride = rgba.stride(0);
    let data = rgba.data(0);
    for row in 0..height as usize {
        let start = row * stride;
        pixels.extend_from_slice(&data[start..start + width as usize * 4]);
    }
    Ok((pixels, width, height))
}

fn orient_rgba(
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    orientation: u16,
) -> Result<(Vec<u8>, u32, u32), StoreError> {
    if orientation <= 1 || orientation > 8 {
        return Ok((pixels, width, height));
    }
    let swaps_dimensions = matches!(orientation, 5..=8);
    let output_width = if swaps_dimensions { height } else { width };
    let output_height = if swaps_dimensions { width } else { height };
    let mut output = vec![0_u8; pixels.len()];
    for y in 0..height {
        for x in 0..width {
            let (destination_x, destination_y) = match orientation {
                2 => (width - 1 - x, y),
                3 => (width - 1 - x, height - 1 - y),
                4 => (x, height - 1 - y),
                5 => (y, x),
                6 => (height - 1 - y, x),
                7 => (height - 1 - y, width - 1 - x),
                8 => (y, width - 1 - x),
                _ => (x, y),
            };
            let source_start = ((y * width + x) * 4) as usize;
            let destination_start = ((destination_y * output_width + destination_x) * 4) as usize;
            output[destination_start..destination_start + 4]
                .copy_from_slice(&pixels[source_start..source_start + 4]);
        }
    }
    Ok((output, output_width, output_height))
}

fn output_dimensions(
    source_width: u32,
    source_height: u32,
    size: ImageReductionSize,
    fit: ImageFit,
) -> Result<(u32, u32, u32, u32), StoreError> {
    let (bound_width, bound_height) = match size {
        ImageReductionSize::MaxEdge(edge) => (edge, edge),
        ImageReductionSize::BoundingBox { width, height } => (width, height),
    };
    let scale = (bound_width as f64 / source_width as f64)
        .min(bound_height as f64 / source_height as f64)
        .min(1.0);
    let output_width = ((source_width as f64 * scale).floor() as u32).max(1);
    let output_height = ((source_height as f64 * scale).floor() as u32).max(1);
    let (canvas_width, canvas_height) = match fit {
        ImageFit::Contain => (output_width, output_height),
        ImageFit::Pad => (bound_width, bound_height),
    };
    if output_width > canvas_width || output_height > canvas_height {
        return Err(StoreError::new(StoreErrorKind::Internal, "Calculated image does not fit its output canvas"));
    }
    Ok((output_width, output_height, canvas_width, canvas_height))
}

fn write_rgba_rows(
    frame: &mut ffmpeg::frame::Video,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<(), StoreError> {
    write_packed_rows(frame, pixels, width, height, 4)
}

fn write_packed_rows(
    frame: &mut ffmpeg::frame::Video,
    pixels: &[u8],
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> Result<(), StoreError> {
    let row_length = width as usize * bytes_per_pixel;
    let stride = frame.stride(0);
    let data = frame.data_mut(0);
    if data.len() < stride.saturating_mul(height as usize) || pixels.len() != row_length * height as usize {
        return Err(StoreError::new(StoreErrorKind::Internal, "Image pixel buffer dimensions do not match"));
    }
    for row in 0..height as usize {
        let source_start = row * row_length;
        let destination_start = row * stride;
        data[destination_start..destination_start + row_length]
            .copy_from_slice(&pixels[source_start..source_start + row_length]);
    }
    Ok(())
}

fn encode_image(
    frame: &ffmpeg::frame::Video,
    format: ReducedFormat,
    quality: usize,
    set_frame_quality: bool,
) -> Result<Vec<u8>, StoreError> {
    let id = encoder_id(format).ok_or_else(|| unsupported("Requested image encoder is unavailable"))?;
    let codec = ffmpeg::encoder::find(id).ok_or_else(|| unsupported("Requested image encoder is unavailable"))?;
    let mut converted_frame = ffmpeg::frame::Video::empty();
    let frame = if format == ReducedFormat::Jpeg {
        let mut scaler = ffmpeg::software::scaling::Context::get(
            frame.format(),
            frame.width(),
            frame.height(),
            ffmpeg::format::Pixel::YUV444P,
            frame.width(),
            frame.height(),
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        )
        .map_err(|error| ffmpeg_error("prepare JPEG color conversion", error))?;
        scaler
            .run(frame, &mut converted_frame)
            .map_err(|error| ffmpeg_error("convert reduced image for JPEG", error))?;
        converted_frame.set_color_range(ffmpeg::color::Range::JPEG);
        if set_frame_quality {
            // The MJPEG encoder reads the quantizer from the submitted frame.
            unsafe { (*converted_frame.as_mut_ptr()).quality = (quality as i32) * ffmpeg::ffi::FF_QP2LAMBDA; }
        }
        &converted_frame
    } else {
        frame
    };
    let mut video_encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|error| ffmpeg_error("create reduced-image encoder", error))?;
    video_encoder.set_width(frame.width());
    video_encoder.set_height(frame.height());
    video_encoder.set_format(frame.format());
    video_encoder.set_time_base((1, 1));
    if format == ReducedFormat::Jpeg {
        video_encoder.set_color_range(ffmpeg::color::Range::JPEG);
        video_encoder.set_quality(quality.saturating_mul(ffmpeg::ffi::FF_QP2LAMBDA as usize));
        video_encoder.set_flags(ffmpeg::codec::Flags::QSCALE);
    }
    let mut encoder = video_encoder
        .open_as(codec)
        .map_err(|error| ffmpeg_error("open reduced-image encoder", error))?;
    encoder
        .send_frame(frame)
        .map_err(|error| ffmpeg_error("encode reduced image", error))?;
    let mut output = Vec::new();
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {
                if let Some(data) = packet.data() {
                    output.extend_from_slice(data);
                }
            }
            Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
            Err(ffmpeg::Error::Eof) => break,
            Err(error) => return Err(ffmpeg_error("read reduced-image output", error)),
        }
    }
    if output.is_empty() {
        return Err(internal("FFmpeg did not produce a reduced image"));
    }
    Ok(output)
}

fn exif_orientation(bytes: &[u8]) -> Option<u16> {
    if let Some(orientation) = tiff_orientation(bytes) {
        return Some(orientation);
    }
    if let Some(exif) = bytes.windows(6).position(|window| window == b"Exif\0\0") && let Some(orientation) = tiff_orientation(&bytes[exif + 6..]) {
        return Some(orientation);
    }
    bytes.windows(4)
        .enumerate()
        .filter(|(_, marker)| marker.starts_with(b"II*\0") || marker.starts_with(b"MM\0*"))
        .find_map(|(offset, _)| tiff_orientation(&bytes[offset..]))
}

fn tiff_orientation(bytes: &[u8]) -> Option<u16> {
    let byte_order = bytes.get(..2)?;
    let little_endian = if byte_order == b"II" {
        true
    } else if byte_order == b"MM" {
        false
    } else {
        return None;
    };
    let read_u16 = |offset: usize| -> Option<u16> {
        let value: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
        Some(if little_endian { u16::from_le_bytes(value) } else { u16::from_be_bytes(value) })
    };
    let read_u32 = |offset: usize| -> Option<u32> {
        let value: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
        Some(if little_endian { u32::from_le_bytes(value) } else { u32::from_be_bytes(value) })
    };
    if read_u16(2)? != 42 {
        return None;
    }
    let directory = usize::try_from(read_u32(4)?).ok()?;
    let entry_count = usize::from(read_u16(directory)?);
    for index in 0..entry_count {
        let entry = directory.checked_add(2)?.checked_add(index.checked_mul(12)?)?;
        if read_u16(entry)? == 0x0112 && read_u16(entry + 2)? == 3 && read_u32(entry + 4)? == 1 {
            let orientation = read_u16(entry + 8)?;
            if (1..=8).contains(&orientation) {
                return Some(orientation);
            }
        }
    }
    None
}

fn ffmpeg_error(operation: &str, error: ffmpeg::Error) -> StoreError {
    internal(format!("Could not {operation}: {error}"))
}

fn internal(detail: impl Into<String>) -> StoreError {
    StoreError::new(StoreErrorKind::Internal, detail)
}

fn unsupported(detail: impl Into<String>) -> StoreError {
    StoreError::new(StoreErrorKind::UnsupportedMediaType, detail)
}

fn capacity(detail: impl Into<String>) -> StoreError {
    StoreError::new(StoreErrorKind::Capacity, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_interface::ImageQuality;

    #[test]
    fn quality_scale_maps_endpoints_and_middle() {
        assert_eq!(ImageQuality::new(100).unwrap().quantizer(), 2);
        assert_eq!(ImageQuality::new(1).unwrap().quantizer(), 31);
        assert_eq!(ImageQuality::new(50).unwrap().quantizer(), 17);
        assert!(ImageQuality::new(0).is_err());
        assert!(ImageQuality::new(101).is_err());
    }

    #[tokio::test]
    async fn quality_changes_jpeg_encoding() {
        let source = Bytes::from_static(include_bytes!("../examples/assets/image.jpg"));
        let high = reencode_image_with_slot(source.clone(), "image/jpeg".into(), ImageQuality::new(100).unwrap(), acquire_image_reduction_slot().await.unwrap()).await.unwrap();
        let low = reencode_image_with_slot(source, "image/jpeg".into(), ImageQuality::new(1).unwrap(), acquire_image_reduction_slot().await.unwrap()).await.unwrap();
        assert_ne!(high.bytes(), low.bytes());
        assert!(high.bytes().len() > low.bytes().len(), "high={} low={}", high.bytes().len(), low.bytes().len());
    }

    #[tokio::test]
    async fn quality_outputs_jpeg_from_transparent_png() {
        let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::RGBA, 2, 2);
        write_rgba_rows(&mut frame, &[255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0], 2, 2).unwrap();
        let png = encode_image(&frame, ReducedFormat::Png, 2, false).unwrap();
        let permit = acquire_image_reduction_slot().await.unwrap();
        let result = reencode_image_with_slot(Bytes::from(png), "image/png".into(), ImageQuality::new(90).unwrap(), permit).await.unwrap();
        assert_eq!(result.content_type(), "image/jpeg");
        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(result.bytes().as_ref()));
        let pixels = decoder.decode().unwrap();
        assert!(pixels.iter().all(|channel| *channel > 230));
    }

    #[tokio::test]
    async fn quality_applies_exif_orientation() {
        let source = include_bytes!("../examples/assets/image.jpg");
        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(source.as_slice()));
        decoder.read_info().unwrap();
        let original = decoder.info().unwrap();
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&6u16.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut oriented = Vec::new();
        oriented.extend_from_slice(&source[..2]);
        oriented.extend_from_slice(&[0xff, 0xe1]);
        oriented.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        oriented.extend_from_slice(b"Exif\0\0");
        oriented.extend_from_slice(&tiff);
        oriented.extend_from_slice(&source[2..]);
        let permit = acquire_image_reduction_slot().await.unwrap();
        let image = reencode_image_with_slot(Bytes::from(oriented), "image/jpeg".into(), ImageQuality::new(80).unwrap(), permit).await.unwrap();
        let mut output_decoder = jpeg_decoder::Decoder::new(Cursor::new(image.bytes().as_ref()));
        output_decoder.read_info().unwrap();
        assert_eq!(output_decoder.info().unwrap().width, original.height);
        assert_eq!(output_decoder.info().unwrap().height, original.width);
    }

    #[tokio::test]
    async fn quality_preserves_dimensions_above_reduction_limit() {
        let width = 2300usize;
        let height = 2usize;
        let row = (width * 3 + 3) & !3;
        let file_len = 54 + row * height;
        let mut bmp = vec![0u8; file_len];
        bmp[..2].copy_from_slice(b"BM");
        bmp[2..6].copy_from_slice(&(file_len as u32).to_le_bytes());
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&(width as u32).to_le_bytes());
        bmp[22..26].copy_from_slice(&(height as u32).to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        for pixel in bmp[54..].as_chunks_mut::<3>().0 { pixel.copy_from_slice(&[40, 80, 120]); }
        let permit = acquire_image_reduction_slot().await.unwrap();
        let image = reencode_image_with_slot(Bytes::from(bmp), "image/bmp".into(), ImageQuality::new(80).unwrap(), permit).await.unwrap();
        assert_eq!(image.content_type(), "image/jpeg");
        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(image.bytes().as_ref()));
        decoder.read_info().unwrap();
        assert_eq!(decoder.info().unwrap().width, width as u16);
        assert_eq!(decoder.info().unwrap().height, height as u16);
    }
}
