//! Media Thumbnails
//!
//! This functionality is gated by 'media_thumbnail', but not at the unit level
//! for historical and simplicity reasons. Instead the feature gates the
//! inclusion of dependencies and nulls out results using the existing interface
//! when not featured.

#[cfg(feature = "media_thumbnail")]
use std::io::Cursor;
use std::{cmp::min, num::Saturating as Sat, sync::Arc, time::Duration};

use bytes::Bytes;
use futures::{StreamExt, pin_mut};
#[cfg(feature = "media_thumbnail")]
use image::{DynamicImage, ImageFormat, ImageReader, Limits, imageops::FilterType};
#[cfg(feature = "media_thumbnail")]
use ruma::http_headers::ContentDispositionType;
use ruma::{Mxc, UInt, UserId, http_headers::ContentDisposition, media::Method};
use tokio::sync::Notify;
#[cfg(feature = "media_thumbnail")]
use tuwunel_core::utils::BoolExt;
use tuwunel_core::{
	Err, Result, async_noinline, checked, err, implement,
	utils::{result::LogDebugErr, stream::IterStream},
};

use super::{Fetched, Media, data::Metadata};

#[cfg(feature = "media_thumbnail")]
pub(super) mod animate;
mod sniff;
#[cfg(all(test, feature = "media_thumbnail"))]
mod tests;

#[cfg(feature = "media_thumbnail")]
use sniff::Sequence;
pub(super) use sniff::{animated_type, animates, sequence};
#[cfg(all(test, feature = "media_thumbnail"))]
use tests::{caller_after_animation, source_fetched};

/// Content type of every thumbnail tuwunel generates.
#[cfg(feature = "media_thumbnail")]
const PNG: &str = "image/png";

/// Bytes the decoder is budgeted per pixel of the picture it is asked for.
#[cfg(feature = "media_thumbnail")]
const BYTES_PER_PIXEL: u64 = 4;

/// Filenames a generated thumbnail is disposed under, per the media repository
/// specification, rather than the name of the file it was generated from.
#[cfg(feature = "media_thumbnail")]
const STILL_NAME: &str = "thumbnail.png";
#[cfg(feature = "media_thumbnail")]
const ANIMATED_NAME: &str = "thumbnail.gif";

/// Content types naming a container that can carry a frame sequence.
///
/// A picture read as animating is stored under the one its header names, so a
/// later lookup holding the key rather than the picture still knows what the
/// row carries.
const APNG: &str = "image/apng";
const GIF: &str = "image/gif";
const WEBP: &str = "image/webp";

/// Content types withheld from a request that asked for a still picture.
///
/// A still `image/webp` cannot be told from an animated one without decoding
/// it, so the family is withheld whole. MSC2705 also names `image/png` for
/// APNG, which cannot join the list because every generated thumbnail is one.
pub(super) const ANIMATED_TYPES: [&str; 3] = [APNG, GIF, WEBP];

/// Dimension specification for a thumbnail.
#[derive(Debug)]
pub struct Dim {
	pub width: u32,
	pub height: u32,
	pub method: Method,
}

/// Whether a thumbnail request will accept an animated result.
///
/// MSC2705 gives the `animated` parameter three states and two behaviors: only
/// `animated=true` may be answered with animation, while `animated=false` and
/// an absent parameter alike forbid it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Animate {
	/// The response must be a still picture.
	#[default]
	Never,

	/// The response may animate.
	Allowed,
}

impl super::Service {
	/// Uploads or replaces a file thumbnail.
	#[tracing::instrument(
		level = "debug",
		ret(level = "debug")
		skip(self, file),
	)]
	pub async fn upload_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		content_disposition: Option<&ContentDisposition>,
		content_type: Option<&str>,
		dim: &Dim,
		file: &[u8],
	) -> Result {
		let key =
			self.db
				.create_file_metadata(mxc, None, dim, content_disposition, content_type)?;

		//TODO: Dangling metadata in database if creation fails
		self.create_media_file(&key, file).await?;
		Ok(())
	}

	/// Answers a thumbnail request, fetching from the peer when it is remote.
	///
	/// The dimension a fetch asks the peer for is the dimension the answer is
	/// filed under, and every later lookup seeks the normalized one, so the
	/// three have to agree or nothing found on one request is found on the
	/// next. Past every bucket there is no dimension to ask at, and the request
	/// names the original file instead.
	#[tracing::instrument(
		level = "debug",
		err(level = "debug")
		skip(self),
	)]
	pub async fn get_or_fetch_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		dim: &Dim,
		animate: Animate,
		timeout_ms: Duration,
		user: &UserId,
	) -> Result<Media> {
		if let Ok(media) = self
			.get_thumbnail(mxc, dim, animate, Some(timeout_ms))
			.await
		{
			return Ok(media);
		}

		if self
			.services
			.globals
			.server_is_ours(mxc.server_name)
		{
			return Err!(Request(NotFound("Local thumbnail not found.")));
		}

		let lock = self.federation_mutex.lock(&mxc.to_string()).await;
		let normalized = dim.normalized();

		if self
			.db
			.file_metadata_exists(mxc, &normalized)
			.await
		{
			drop(lock);
			return self.get_thumbnail(mxc, dim, animate, None).await;
		}

		let fetched = match normalized.is_original() {
			| true =>
				self.fetch_remote_content(mxc, None, timeout_ms)
					.await?,
			| false =>
				self.fetch_remote_thumbnail(mxc, None, timeout_ms, &normalized, animate)
					.await?,
		};

		// a peer may ignore the parameter and this answers the caller directly,
		// so the variant is repaired before it leaves rather than one request on
		if animate.accepts_fetched(&fetched) {
			return Ok(fetched.media);
		}

		self.store_still(mxc, &normalized, fetched.media)
			.await
	}

	/// Download a thumbnail and wait up to a timeout_ms if it is pending.
	///
	/// The future is boxed because the still-repair path pulls the thumbnailer
	/// into it, and inlining that into every caller overflows the layout depth
	/// limit in the federation handler.
	#[async_noinline]
	#[tracing::instrument(
		level = "debug",
		err(level = "debug")
		skip(self),
	)]
	pub async fn get_thumbnail<'a>(
		&'a self,
		mxc: &'a Mxc<'_>,
		dim: &'a Dim,
		animate: Animate,
		timeout_duration: Option<Duration>,
	) -> Result<Media> {
		if let Ok(meta) = self.get_stored_thumbnail(mxc, dim, animate).await {
			return Ok(meta);
		}

		let Some(timeout_duration) = timeout_duration else {
			return Err!(Request(NotFound("Media thumbnail not found.")));
		};

		let Ok(_pending) = self.db.search_pending_mxc(mxc).await else {
			return Err!(Request(NotFound("Media thumbnail not found.")));
		};

		let notifier = self
			.mxc_state
			.notifiers
			.lock()?
			.entry(mxc.to_string().into())
			.or_insert_with(|| Arc::new(Notify::new()))
			.clone();

		if tokio::time::timeout(timeout_duration, notifier.notified())
			.await
			.is_err()
		{
			return Err!(Request(NotYetUploaded("Media has not been uploaded yet")));
		}

		self.get_stored_thumbnail(mxc, dim, animate).await
	}

	/// Downloads a file's thumbnail.
	///
	/// Here's an example on how it works:
	///
	/// - Client requests an image with width=567, height=567
	/// - Server rounds that up to (800, 600), so it doesn't have to save too
	///   many thumbnails
	/// - Server rounds that up again to (958, 600) to fix the aspect ratio
	///   (only for width,height>96)
	/// - Server creates the thumbnail and sends it to the user
	///
	/// For width,height <= 96 the server uses another thumbnailing algorithm
	/// which crops the image afterwards.
	#[tracing::instrument(
		name = "thumbnail",
		level = "debug",
		err(level = "trace")
		skip(self),
	)]
	pub async fn get_stored_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		dim: &Dim,
		animate: Animate,
	) -> Result<Media> {
		let dim = dim.normalized();

		// the sentinel is the key the original is stored under rather than a
		// size, so a request reaching it is answered from the original itself
		if dim.is_original() {
			return self.answer_original(mxc, animate).await;
		}

		if let Ok(metadata) = self
			.db
			.search_file_metadata(mxc, &dim, animate)
			.await
		{
			return self
				.answer_stored(mxc, &dim, animate, metadata)
				.await;
		}

		let Ok(metadata) = self.original_metadata(mxc).await else {
			return self.answer_promoted(mxc, animate).await;
		};

		self.get_thumbnail_generate(mxc, &dim, animate, metadata)
			.await
	}
}

/// Answers a request past every bucket, which names the original file.
///
/// The original's own row is keyed at the sentinel, where there is no size to
/// re-encode at, so a picture the request will not accept stands in at its own
/// dimensions. Those are the largest a still may carry without upscaling, so
/// the stand-in covers any request the original itself covers and is the best
/// available where it does not.
///
/// The future is not inlined: this branch reaches the thumbnailer, and folding
/// that into every caller overflows the layout depth limit.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[async_noinline]
#[tracing::instrument(name = "original", level = "debug", skip(self))]
async fn answer_original<'a>(&'a self, mxc: &'a Mxc<'_>, animate: Animate) -> Result<Media> {
	let Ok(data) = self.original_metadata(mxc).await else {
		return self.answer_promoted(mxc, animate).await;
	};

	let bytes = self.fetch_bytes(&data.key).await?;

	if animate.accepts_picture(&bytes) {
		return Ok(into_media(data, bytes.into()));
	}

	let dim = picture_dim(&bytes);

	let stored = self
		.db
		.search_file_metadata(mxc, &dim, animate)
		.await
		.ok();

	if let Some(metadata) = stored {
		drop((bytes, data));

		return self
			.answer_stored(mxc, &dim, animate, metadata)
			.await;
	}

	// no still can be derived from a picture that will not decode, and every
	// bucket answers that with the original rather than refusing
	let Ok(image) = self.decode(&bytes) else {
		return match animate.accepts_fallback(&bytes) {
			| true => Ok(into_media(data, bytes.into())),
			| false => Err!(Request(NotFound("Media thumbnail not found."))),
		};
	};

	drop((bytes, data));

	self.store_thumbnail(mxc, &dim, image).await
}

/// Hands the original back, there being no thumbnailer to stand a still in.
///
/// A build without the feature keeps serving what it holds rather than
/// refusing, so a request that forbade animation goes unhonored here.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::Service)]
#[tracing::instrument(name = "original", level = "debug", skip_all)]
async fn answer_original(&self, mxc: &Mxc<'_>, animate: Animate) -> Result<Media> {
	let Ok(data) = self.original_metadata(mxc).await else {
		return self.answer_promoted(mxc, animate).await;
	};

	self.get_thumbnail_saved(data).await
}

/// Metadata for the original file, which every thumbnail is derived from.
///
/// Its row is keyed at the sentinel dimension, and nothing is withheld from
/// this lookup: the row is the original rather than a variant of it.
#[implement(super::Service)]
async fn original_metadata(&self, mxc: &Mxc<'_>) -> Result<Metadata> {
	self.db
		.search_file_metadata(mxc, &Dim::default(), Animate::Allowed)
		.await
}

/// Answers from stored media when no metadata row names the original.
///
/// The original may be lazy preview media promoted on this very request, which
/// leaves no row behind, and only a picture is worth serving in a thumbnail's
/// place. No row having named it, the request's own preference is the only gate
/// it passes, so the picture is read here as it is anywhere else.
#[implement(super::Service)]
#[tracing::instrument(name = "promoted", level = "debug", skip(self))]
async fn answer_promoted(&self, mxc: &Mxc<'_>, animate: Animate) -> Result<Media> {
	let media = self.get_stored(mxc).await?;
	let servable = media
		.content_type
		.as_deref()
		.is_some_and(|content_type| content_type.starts_with("image/"))
		&& animate.accepts_picture(&media.content);

	servable
		.then_some(media)
		.ok_or_else(|| err!(Request(NotFound("Media not found."))))
}

/// The stored bytes for a thumbnail row, from the first provider holding them.
///
/// Returning the shared `Bytes` spares a caller that only reads the picture
/// the `Vec` copy `Media` would force on it.
#[implement(super::Service)]
#[tracing::instrument(name = "fetch", level = "debug", skip_all)]
async fn fetch_bytes(&self, key: &[u8]) -> Result<Bytes> {
	#[cfg(all(test, feature = "media_thumbnail"))]
	source_fetched();

	let path = self.get_media_name_sha256(key);
	let fetch = self
		.storage_providers()
		.stream()
		.filter_map(async |provider| {
			provider
				.get(path.as_str())
				.await
				.log_debug_err()
				.ok()
		});

	pin_mut!(fetch);

	fetch
		.next()
		.await
		.ok_or_else(|| err!(Request(NotFound("Media not found."))))
}

/// The size a picture that may not be served stands in at.
///
/// Only the header is read, since the decode this precedes may never happen.
/// A header that will not read at all falls back to the largest bucket, which
/// answers smaller than the request but never with the animation.
#[cfg(feature = "media_thumbnail")]
fn picture_dim(bytes: &[u8]) -> Dim { header_dim(bytes).unwrap_or_else(Dim::largest) }

/// The dimensions this picture's own header declares.
///
/// A header that will not read, or that reads as having no extent, answers
/// `None` rather than a size nothing could be encoded at. Callers that must
/// name a size regardless take [`picture_dim`] instead.
#[cfg(feature = "media_thumbnail")]
fn header_dim(bytes: &[u8]) -> Option<Dim> {
	reader(bytes)
		.ok()
		.and_then(|reader| reader.into_dimensions().ok())
		.filter(|&(width, height)| width > 0 && height > 0)
		.map(|(width, height)| Dim::new(width, height, Some(Method::Scale)))
}

/// Answers a request from a stored row, re-deriving a still if it animates.
///
/// The type a row is stored under is whatever produced it claimed, and a peer
/// is free to claim wrongly, so the picture itself decides once it is in hand.
/// A row that may not answer is re-encoded and the still left behind for the
/// next request.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "stored", level = "debug", skip(self, data))]
async fn answer_stored(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	animate: Animate,
	data: Metadata,
) -> Result<Media> {
	let bytes = self.fetch_bytes(&data.key).await?;

	if animate.accepts_picture(&bytes) {
		return Ok(into_media(data, bytes.into()));
	}

	let image = self.decode_still(&bytes)?;

	drop((bytes, data));

	self.store_thumbnail(mxc, dim, image).await
}

/// Hands the stored row back whatever its picture holds.
///
/// Judging it would only be worth doing if a still could then be derived, and
/// a build without the feature has no thumbnailer to derive one with.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::Service)]
#[tracing::instrument(name = "stored", level = "debug", skip_all)]
async fn answer_stored(
	&self,
	_mxc: &Mxc<'_>,
	_dim: &Dim,
	_animate: Animate,
	data: Metadata,
) -> Result<Media> {
	self.get_thumbnail_saved(data).await
}

/// Answers a thumbnail row from the picture already in storage.
///
/// The response hands its body over by value, so the bytes are converted
/// rather than read in place; the conversion reclaims the storage buffer
/// whenever this holds the only handle to it.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::Service)]
#[tracing::instrument(name = "saved", level = "debug", skip_all)]
async fn get_thumbnail_saved(&self, data: Metadata) -> Result<Media> {
	let bytes = self.fetch_bytes(&data.key).await?;

	Ok(into_media(data, bytes.into()))
}

/// Decodes a picture that may not be served in the state it is held in.
///
/// Serving it is what the request forbade, so a decode that fails leaves
/// nothing answerable and the caller has no fallback to offer.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
fn decode_still(&self, bytes: &[u8]) -> Result<DynamicImage> {
	self.decode(bytes)
		.map_err(|_| err!(Request(NotFound("Media thumbnail not found."))))
}

/// Re-encode a fetched picture as a still thumbnail and store it.
///
/// A peer that ignores the parameter answers a cold fetch with animation, and
/// this repairs it on the way out rather than one request later. The cached
/// row takes its own path, which already holds the picture.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "still", level = "debug", skip(self, animated))]
pub(super) async fn store_still(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	animated: Media,
) -> Result<Media> {
	// the sentinel names the original rather than a size to re-encode at, so
	// the picture stands in at its own, as it does on the lookup path
	let standin = dim
		.is_original()
		.then(|| picture_dim(&animated.content));

	let image = self.decode_still(&animated.content)?;

	drop(animated);

	self.store_thumbnail(mxc, standin.as_ref().unwrap_or(dim), image)
		.await
}

/// Hands the picture back as it stands, there being no thumbnailer.
///
/// A build without the feature keeps serving what it holds rather than
/// refusing, so a request for a still goes unhonored instead of failing.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::Service)]
#[tracing::instrument(name = "still", level = "debug", skip_all)]
pub(super) async fn store_still(
	&self,
	_mxc: &Mxc<'_>,
	_dim: &Dim,
	animated: Media,
) -> Result<Media> {
	Ok(animated)
}

/// Generate a thumbnail.
///
/// A source that animates yields both variants here rather than the one this
/// request asked for. Generation runs only on a lookup miss, and the row it
/// stores is what stops the next miss, so a variant left ungenerated would wait
/// on a miss that the other variant has already made impossible.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "generate", level = "debug", skip(self, data))]
async fn get_thumbnail_generate(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	animate: Animate,
	data: Metadata,
) -> Result<Media> {
	let animation_enabled = self.services.config.media_thumbnail_animated;
	let admission = animation_enabled
		.then_async(async || {
			let slots = self.animated_thumbnail_slots.clone();
			let Ok(admission) = slots.acquire_owned().await else {
				return Err!(debug_warn!("The animated thumbnail semaphore is closed."));
			};

			Ok(admission)
		})
		.await
		.transpose()?;

	let bytes = self.fetch_bytes(&data.key).await?;

	// the gates below read this walk too, so it is taken at most once
	let walk = animation_enabled.then(|| sequence(&bytes));

	let (encoded, admission) = if animates_at(dim, walk, &bytes)? {
		let permit = admission.expect("animation work has source admission");
		let output = self
			.store_animated(mxc, dim, bytes.clone(), permit)
			.await?;

		#[cfg(test)]
		caller_after_animation().await;

		(output.media.ok(), Some(output.admission))
	} else {
		(None, admission)
	};

	// the variant this request did not ask for is left stored rather than held,
	// so its buffer is not carried across the still encode below
	let animated = animate.allowed().and(encoded);

	let media = into_media(data, bytes.into());
	let frame = self.video_frame(mxc, dim, &media).await;
	let from_video = frame.is_some();

	let Ok(image) = self.decode(frame.as_deref().unwrap_or(&media.content)) else {
		// a frame the thumbnailer refuses is this video's verdict too; without
		// it the program would run again on the next request for any size
		if from_video {
			self.remember_failure(mxc);
		}

		if let Some(animated) = animated {
			return Ok(animated);
		}

		// no still can be derived from a picture that will not decode, and the
		// original answers in its place unless it is the animation the request
		// forbade
		let named = walk.map(Sequence::names_animation);

		return match animate.accepts_fallback_walk(named, &media.content) {
			| true => Ok(media),
			| false => Err!(Request(NotFound("Media thumbnail not found."))),
		};
	};

	drop(frame);

	// a video is never servable in place of its own thumbnail, so its frame is
	// re-encoded however small it is
	let source = Dim::new(image.width(), image.height(), None);
	let animates = walk.map(Sequence::animates);

	if !from_video
		&& dim.is_passthrough(&source)?
		&& animate.accepts_walk(animates, &media.content)
	{
		return Ok(media);
	}

	// nothing below reads the original, which on the video path is the whole
	// staged file, and the encode and the store must not hold it
	drop(media);

	let still = self.store_thumbnail(mxc, dim, image).await?;

	drop(admission);

	Ok(animated.unwrap_or(still))
}

/// Hands the original back in place of the thumbnail it cannot generate.
///
/// The row this is given is the original's own, so the caller is answered with
/// media the server holds rather than being refused outright.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::Service)]
#[tracing::instrument(name = "fallback", level = "debug", skip_all)]
async fn get_thumbnail_generate(
	&self,
	_mxc: &Mxc<'_>,
	_dim: &Dim,
	_animate: Animate,
	data: Metadata,
) -> Result<Media> {
	self.get_thumbnail_saved(data).await
}

/// Whether this source yields an animated variant at these dimensions.
///
/// Only a picture whose header says it holds a frame sequence reaches the
/// encoder, so a still never pays a decode that would yield one frame and be
/// thrown away, and a video is excluded by the same test since its container is
/// none of the three that hold frames. A size the request cannot improve on is
/// passed through whole below rather than re-encoded. The caller takes the
/// walk and reads it again at that passthrough, and hands `None` where the
/// feature is off.
#[cfg(feature = "media_thumbnail")]
fn animates_at(dim: &Dim, walk: Option<Sequence>, content: &[u8]) -> Result<bool> {
	if !walk.is_some_and(Sequence::animates) {
		return Ok(false);
	}

	// the passthrough below is tested against the decoded size, so a header
	// that will not read would leave the two disagreeing over one picture
	let Some(source) = header_dim(content) else {
		return Ok(false);
	};

	dim.is_passthrough(&source)
		.map(|through| !through)
}

/// Encode a still PNG thumbnail at these dimensions and store it.
///
/// The generate path and the still-repair path share this, so a given size
/// carries one content type and one disposition whichever produced it.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "store", level = "debug", skip(self, image))]
async fn store_thumbnail(&self, mxc: &Mxc<'_>, dim: &Dim, image: DynamicImage) -> Result<Media> {
	let mut content = Vec::new();
	let thumbnail = thumbnail_generate(&image, dim)?;

	// the source raster is dead once the thumbnail exists, and neither it nor
	// the thumbnail may be held across the store below
	drop(image);

	let mut cursor = Cursor::new(&mut content);

	thumbnail
		.write_to(&mut cursor, ImageFormat::Png)
		.map_err(|error| err!(error!(?error, "Error writing PNG thumbnail.")))?;

	drop(thumbnail);

	self.store_encoded(mxc, dim, content, PNG, STILL_NAME)
		.await
}

/// Store an encoded thumbnail under the type and name it carries.
///
/// Both encoders end here, so a stored thumbnail is disposed inline under the
/// name the media repository specification asks of one whether or not the
/// original arrived with a name of its own.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "encoded", level = "debug", skip(self, content))]
async fn store_encoded(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	content: Vec<u8>,
	content_type: &str,
	filename: &str,
) -> Result<Media> {
	let content_disposition = ContentDisposition {
		disposition_type: ContentDispositionType::Inline,
		filename: Some(filename.to_owned()),
	};

	let key = self.db.create_file_metadata(
		mxc,
		None,
		dim,
		Some(&content_disposition),
		Some(content_type),
	)?;

	self.create_media_file(&key, &content).await?;

	let media = Media {
		content,
		content_type: Some(content_type.to_owned()),
		content_disposition: Some(content_disposition),
	};

	Ok(media)
}

/// Decode a picture whose header declares no more than the configured pixel
/// count. The dimensions are checked before any decoder allocates, since
/// `Limits` enforces only a byte budget and leaves a decoder free to ignore it.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "decode", level = "trace", skip_all)]
fn decode(&self, bytes: &[u8]) -> Result<DynamicImage> {
	let budget = self.services.config.media_thumbnail_max_pixels;
	let (width, height) = reader(bytes)?
		.into_dimensions()
		.map_err(|error| err!(debug_warn!(?error, "Failed to read picture dimensions.")))?;

	let pixels = u64::from(width).saturating_mul(u64::from(height));

	if pixels > budget {
		return Err!(debug_warn!(%width, %height, "Picture is past the {budget} pixel budget."));
	}

	let mut limits = Limits::no_limits();
	limits.max_alloc = Some(budget.saturating_mul(BYTES_PER_PIXEL));

	let mut reader = reader(bytes)?;
	reader.limits(limits);

	reader
		.decode()
		.map_err(|error| err!(debug_warn!(?error, "Failed to decode picture.")))
}

#[cfg(feature = "media_thumbnail")]
fn reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>> {
	ImageReader::new(Cursor::new(bytes))
		.with_guessed_format()
		.map_err(Into::into)
}

#[cfg(feature = "media_thumbnail")]
pub(super) fn thumbnail_generate(image: &DynamicImage, requested: &Dim) -> Result<DynamicImage> {
	let thumbnail = if !requested.crop() {
		let Dim { width, height, .. } = requested.scaled(&Dim {
			width: image.width(),
			height: image.height(),
			..Dim::default()
		})?;
		image.thumbnail_exact(width, height)
	} else {
		// upscaling is forbidden outright, and resize_to_fill enlarges a source
		// smaller than the request to meet it
		let width = min(requested.width, image.width());
		let height = min(requested.height, image.height());

		image.resize_to_fill(width, height, FilterType::CatmullRom)
	};

	Ok(thumbnail)
}

fn into_media(data: Metadata, content: Vec<u8>) -> Media {
	Media {
		content,
		content_type: data.content_type,
		content_disposition: data.content_disposition,
	}
}

impl Animate {
	/// Returns true when the request will accept animation.
	///
	/// Only `animated=true` reaches this state; `animated=false` and an absent
	/// parameter alike forbid animation.
	#[inline]
	#[must_use]
	pub fn allowed(self) -> bool { matches!(self, Self::Allowed) }

	/// Returns true when content of this type may answer the request at all.
	///
	/// This reads the declared type, which whoever uploaded the picture chose,
	/// so it is only for deciding between stored rows, where the pictures
	/// themselves are not in hand. Prefer [`Self::accepts_picture`] anywhere
	/// the bytes are.
	#[inline]
	#[must_use]
	pub fn accepts_type(self, content_type: Option<&str>) -> bool {
		self.allowed() || !content_type.is_some_and(declares_animation)
	}

	/// Returns true when this type is the variant the request asked for.
	///
	/// A source that animates leaves both variants stored at a size, so which
	/// one a lookup answers with must be stated rather than left to the order
	/// their keys fall in, which a remote row's own disposition decides. The
	/// other variant still answers when it is the only one there, so this
	/// orders the rows rather than refusing any of them.
	#[inline]
	#[must_use]
	pub fn prefers_type(self, content_type: Option<&str>) -> bool {
		self.allowed() == content_type.is_some_and(declares_animation)
	}

	/// Returns true when this picture may answer the request.
	///
	/// The bytes decide, so a file cannot pass a request that forbade
	/// animation by declaring a content type that does not animate, and an
	/// APNG is caught despite being an `image/png` like every thumbnail.
	#[inline]
	#[must_use]
	pub fn accepts_picture(self, bytes: &[u8]) -> bool { self.allowed() || !animates(bytes) }

	/// Returns true when a picture may answer, given what a walk settled about
	/// it.
	///
	/// The walk that settles this is the same one picking the type a fetched
	/// row is filed under, so a caller already holding its answer states it
	/// here rather than reading the same bytes again through
	/// [`Self::accepts_picture`].
	#[inline]
	#[must_use]
	fn accepts_animation(self, animates: bool) -> bool { self.allowed() || !animates }

	/// Returns true when a fetched picture may answer, walking it if nobody
	/// has.
	///
	/// Filing a row settles this on the way, and a redirect files none, so the
	/// walk that was skipped there happens here for the one caller that asks.
	#[must_use]
	pub fn accepts_fetched(self, fetched: &Fetched) -> bool {
		self.accepts_walk(fetched.animates, &fetched.media.content)
	}

	/// Returns true when this picture may answer in a thumbnail's place.
	///
	/// Nothing can be derived from a picture that will not decode, so the
	/// choice is between the original and refusing media the server holds. Only
	/// a walk that settled on animation withholds it here, where
	/// [`Self::accepts_picture`] withholds anything it could not settle, since
	/// refusing every unreadable still would cost more than it buys.
	#[inline]
	#[must_use]
	pub fn accepts_fallback(self, bytes: &[u8]) -> bool {
		self.allowed() || animated_type(bytes).is_none()
	}

	/// Returns true when this picture may answer, walking it if nobody has.
	///
	/// A caller already holding a walk of these bytes states what it settled
	/// rather than paying for a second one over them, and where none was taken
	/// this is [`Self::accepts_picture`] exactly.
	#[inline]
	#[must_use]
	pub(super) fn accepts_walk(self, animates: Option<bool>, bytes: &[u8]) -> bool {
		animates.map_or_else(
			|| self.accepts_picture(bytes),
			|animates| self.accepts_animation(animates),
		)
	}

	/// Returns true when this picture may answer in a thumbnail's place,
	/// walking it if nobody has.
	///
	/// The settled-only rule of [`Self::accepts_fallback`] holds here too, so
	/// what the caller states is whether its walk *named* an animation rather
	/// than whether it withheld one.
	#[cfg(feature = "media_thumbnail")]
	#[inline]
	#[must_use]
	pub(super) fn accepts_fallback_walk(self, names: Option<bool>, bytes: &[u8]) -> bool {
		names.map_or_else(|| self.accepts_fallback(bytes), |names| self.allowed() || !names)
	}
}

impl From<Option<bool>> for Animate {
	fn from(animated: Option<bool>) -> Self {
		match animated {
			| Some(true) => Self::Allowed,
			| Some(false) | None => Self::Never,
		}
	}
}

impl From<Animate> for Option<bool> {
	fn from(animate: Animate) -> Self { Some(animate.allowed()) }
}

/// Whether a declared content type names an animating container.
///
/// This reads what an upload claimed rather than the picture itself, so it
/// decides only between stored rows, where no picture is in hand. The reader
/// that decides from the bytes is `sniff::animates`.
fn declares_animation(content_type: &str) -> bool {
	let essence = content_type
		.split_once(';')
		.map_or(content_type, |(essence, _)| essence)
		.trim();

	ANIMATED_TYPES
		.iter()
		.any(|kind| essence.eq_ignore_ascii_case(kind))
}

impl Dim {
	/// Instantiate a Dim from Ruma integers with optional method.
	pub fn from_ruma(width: UInt, height: UInt, method: Option<Method>) -> Result<Self> {
		let width = width
			.try_into()
			.map_err(|e| err!(Request(InvalidParam("Width is invalid: {e:?}"))))?;
		let height = height
			.try_into()
			.map_err(|e| err!(Request(InvalidParam("Height is invalid: {e:?}"))))?;

		Ok(Self::new(width, height, method))
	}

	/// Instantiate a Dim with optional method
	#[inline]
	#[must_use]
	pub fn new(width: u32, height: u32, method: Option<Method>) -> Self {
		Self {
			width,
			height,
			method: method.unwrap_or(Method::Scale),
		}
	}

	pub fn scaled(&self, image: &Self) -> Result<Self> {
		let image_width = image.width;
		let image_height = image.height;

		let width = min(self.width, image_width);
		let height = min(self.height, image_height);

		let use_width = Sat(width) * Sat(image_height) < Sat(height) * Sat(image_width);

		let x = if use_width {
			let dividend = (Sat(height) * Sat(image_width)).0;
			checked!(dividend / image_height)?
		} else {
			width
		};

		let y = if !use_width {
			let dividend = (Sat(width) * Sat(image_height)).0;
			checked!(dividend / image_width)?
		} else {
			height
		};

		Ok(Self {
			width: x,
			height: y,
			method: Method::Scale,
		})
	}

	/// Returns true when generation cannot improve on the source and the
	/// original should be served instead: either the request would upscale, or
	/// the generated thumbnail would carry the source's own dimensions.
	pub fn is_passthrough(&self, source: &Self) -> Result<bool> {
		if self.width > source.width || self.height > source.height {
			return Ok(true);
		}

		let (width, height) = if self.crop() {
			(self.width, self.height)
		} else {
			let scaled = self.scaled(source)?;
			(scaled.width, scaled.height)
		};

		Ok(width == source.width && height == source.height)
	}

	/// The size a requested one is answered at.
	///
	/// Bucketing keeps the number of stored variants bounded, so the requested
	/// method is discarded along with the requested size. A request above every
	/// bucket answers the sentinel, which stands for the original file.
	#[must_use]
	pub fn normalized(&self) -> Self {
		match (self.width, self.height) {
			| (0..=32, 0..=32) => Self::new(32, 32, Some(Method::Crop)),
			| (0..=96, 0..=96) => Self::new(96, 96, Some(Method::Crop)),
			| (0..=320, 0..=240) => Self::new(320, 240, Some(Method::Scale)),
			| (0..=640, 0..=480) => Self::new(640, 480, Some(Method::Scale)),
			| (0..=800, 0..=600) => Self::largest(),
			| _ => Self::default(),
		}
	}

	/// The largest size a thumbnail is generated at.
	///
	/// Every request above this normalizes to the sentinel instead, so it is
	/// also the fallback for a picture whose own size cannot be read.
	#[inline]
	#[must_use]
	pub fn largest() -> Self { Self::new(800, 600, Some(Method::Scale)) }

	/// Returns true if the method is Crop.
	#[inline]
	#[must_use]
	pub fn crop(&self) -> bool { self.method == Method::Crop }

	/// Returns true for the sentinel that stands for the original file.
	///
	/// Every request too large to thumbnail normalizes to zero by zero, which
	/// is also the key the original itself is stored under, so nothing may be
	/// withheld there.
	#[inline]
	#[must_use]
	pub fn is_original(&self) -> bool { self.width == 0 && self.height == 0 }
}

impl Default for Dim {
	#[inline]
	fn default() -> Self {
		Self {
			width: 0,
			height: 0,
			method: Method::Scale,
		}
	}
}
