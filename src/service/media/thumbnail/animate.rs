//! Animated thumbnail encoding
//!
//! A picture that carries a frame sequence can answer a request for an animated
//! thumbnail with the frames themselves rather than one of them. The three
//! containers MSC2705 names each hold a sequence differently, so each has its
//! own decoder here, and all of them produce a GIF, which is the one animated
//! format this build can encode.
//!
//! Every frame is quantized onto a palette of its own, so the work is bounded
//! twice over, by a frame count and by a pixel budget spent across them, and
//! runs on a blocking worker rather than the async runtime.

use std::{io::Cursor, num::Saturating as Sat};

use bytes::Bytes;
use image::{
	AnimationDecoder, DynamicImage, Frame, Frames, ImageDecoder, ImageFormat, Limits,
	codecs::{
		gif::{GifDecoder, GifEncoder, Repeat},
		png::PngDecoder,
		webp::WebPDecoder,
	},
};
use ruma::Mxc;
use tuwunel_core::{Err, Result, defer, err, implement};

use super::{ANIMATED_NAME, BYTES_PER_PIXEL, Dim, GIF, Media, reader, thumbnail_generate};

/// Quantization effort the GIF encoder spends per frame.
///
/// The encoder's own default is 1, which buys quality at any cost; a thumbnail
/// is small and any remote server can ask for one, so this trades a little of
/// that for an encode that ends.
const GIF_SPEED: i32 = 10;

/// Encode this picture's frames as an animated GIF thumbnail and store it.
///
/// Quantization runs per frame on a palette of its own, which is far more work
/// than a still costs, so the encode is handed to a blocking worker rather than
/// run on the async runtime. A source whose frames will not decode answers an
/// error and the caller falls through to the still it would have produced.
#[implement(super::super::Service)]
#[tracing::instrument(name = "animate", level = "debug", skip(self, source))]
pub(super) async fn store_animated(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	source: Bytes,
) -> Result<Media> {
	let config = &self.services.config;
	let max_frames = config.media_thumbnail_max_frames;
	let budget = config.media_thumbnail_max_pixels;
	let requested = Dim::new(dim.width, dim.height, Some(dim.method.clone()));

	let encode = self
		.services
		.server
		.runtime()
		.spawn_blocking(move || encode_frames(&source, &requested, max_frames, budget));

	// a started worker runs to completion, but one still queued is dropped, so
	// a cancelled request must not leave it to reach the pool
	let abort = encode.abort_handle();

	defer! {{ abort.abort(); }}

	let content = encode.await??;

	self.store_encoded(mxc, dim, content, GIF, ANIMATED_NAME)
		.await
}

/// Encode a picture's frames as a GIF scaled to these dimensions.
///
/// The frame iterator is lazy and its decoder is given the pixel budget before
/// it advances, so the caps bound the decode as well as the encode: frames stop
/// at the count limit or once their source pixels have spent the budget, and
/// the animation loops short rather than the request failing. Fewer than two
/// frames is no animation and answers an error.
pub(in super::super) fn encode_frames(
	bytes: &[u8],
	dim: &Dim,
	max_frames: usize,
	budget: u64,
) -> Result<Vec<u8>> {
	let frames = source_frames(bytes, budget)?;
	let mut content = Vec::new();
	let mut encoder = GifEncoder::new_with_speed(&mut content, GIF_SPEED);
	let mut spent = Sat(0_u64);
	let mut count = 0_usize;

	encoder
		.set_repeat(Repeat::Infinite)
		.map_err(|error| err!(debug_warn!(?error, "Failed to set the GIF loop count.")))?;

	for frame in frames.take(max_frames) {
		let frame =
			frame.map_err(|error| err!(debug_warn!(?error, "Failed to decode a frame.")))?;

		let delay = frame.delay();
		let buffer = frame.into_buffer();

		spent += Sat(u64::from(buffer.width())) * Sat(u64::from(buffer.height()));

		if spent.0 > budget {
			break;
		}

		let scaled = thumbnail_generate(&DynamicImage::ImageRgba8(buffer), dim)?;

		encoder
			.encode_frame(Frame::from_parts(scaled.into_rgba8(), 0, 0, delay))
			.map_err(|error| err!(debug_warn!(?error, "Failed to encode a frame.")))?;

		count = count.saturating_add(1);
	}

	// the encoder writes the trailer as it goes out of scope, so the picture is
	// not complete until it has
	drop(encoder);

	if count < 2 {
		return Err!(debug_warn!(%count, "Picture carries no frame sequence."));
	}

	Ok(content)
}

/// The frame sequence a picture carries, by the format its header names.
///
/// Only the three containers MSC2705 names can hold one, and a WebP says in its
/// own header whether it does, so anything else answers an error here rather
/// than yielding a lone frame for the caller to reject.
fn source_frames(bytes: &[u8], budget: u64) -> Result<Frames<'_>> {
	let format = reader(bytes)?
		.format()
		.ok_or_else(|| err!(debug_warn!("Picture names no format.")))?;

	// each decoder here is built with no limits of its own and decodes a whole
	// source frame on the first advance, so the header is budgeted first
	let (width, height) = reader(bytes)?
		.into_dimensions()
		.map_err(|error| err!(debug_warn!(?error, "Failed to read picture dimensions.")))?;

	let pixels = u64::from(width).saturating_mul(u64::from(height));

	if pixels > budget {
		return Err!(debug_warn!(%width, %height, "Frames are past the {budget} pixel budget."));
	}

	let mut limits = Limits::no_limits();
	limits.max_alloc = Some(budget.saturating_mul(BYTES_PER_PIXEL));

	let source = Cursor::new(bytes);
	let failed = |error| err!(debug_warn!(?error, ?format, "Failed to read a frame sequence."));

	let frames = match format {
		| ImageFormat::Gif => {
			let mut decoder = GifDecoder::new(source).map_err(failed)?;

			decoder.set_limits(limits).map_err(failed)?;
			decoder.into_frames()
		},
		| ImageFormat::Png => {
			let mut decoder = PngDecoder::new(source).map_err(failed)?;

			decoder.set_limits(limits).map_err(failed)?;
			decoder.apng().map_err(failed)?.into_frames()
		},
		| ImageFormat::WebP => {
			let mut decoder = WebPDecoder::new(source).map_err(failed)?;

			if !decoder.has_animation() {
				return Err!(debug_warn!("WebP carries a single frame."));
			}

			decoder.set_limits(limits).map_err(failed)?;
			decoder.into_frames()
		},
		| _ => return Err!(debug_warn!(?format, "Format carries no frame sequence.")),
	};

	Ok(frames)
}
