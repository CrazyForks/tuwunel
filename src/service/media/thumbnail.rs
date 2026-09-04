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
use tuwunel_core::{
	Err, Result, async_noinline, checked, err, implement,
	utils::{result::LogDebugErr, stream::IterStream},
};

use super::{Media, data::Metadata};

/// Content type of every thumbnail tuwunel generates.
#[cfg(feature = "media_thumbnail")]
const PNG: &str = "image/png";

/// Bytes the decoder is budgeted per pixel of the picture it is asked for.
#[cfg(feature = "media_thumbnail")]
const BYTES_PER_PIXEL: u64 = 4;

/// Filename a generated thumbnail is disposed under, per the media repository
/// specification, rather than the name of the file it was generated from.
#[cfg(feature = "media_thumbnail")]
const THUMBNAIL_NAME: &str = "thumbnail.png";

/// Content types withheld from a request that asked for a still picture.
///
/// A still `image/webp` cannot be told from an animated one without decoding
/// it, so the family is withheld whole. MSC2705 also names `image/png` for
/// APNG, which cannot join the list because every generated thumbnail is one.
pub(super) const ANIMATED_TYPES: [&str; 3] = ["image/apng", "image/gif", "image/webp"];

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

		let media = self
			.fetch_remote_thumbnail(mxc, None, timeout_ms, dim, animate)
			.await?;

		// a peer may ignore the parameter and this answers the caller directly,
		// so the variant is repaired before it leaves rather than one request on
		if animate.accepts_picture(&media.content) {
			return Ok(media);
		}

		self.store_still(mxc, &normalized, media).await
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
		// 0, 0 because that's the original file
		let dim = dim.normalized();
		let animate = animate.at(&dim);

		if let Ok(metadata) = self
			.db
			.search_file_metadata(mxc, &dim, animate)
			.await
		{
			// a peer ignoring the animated parameter answers with an animated
			// passthrough, which is re-derived here rather than refused
			return match animate.accepts_type(metadata.content_type.as_deref()) {
				| true => self.get_thumbnail_saved(metadata).await,
				| false =>
					self.get_thumbnail_still(mxc, &dim, metadata)
						.await,
			};
		}

		// the original may be lazy preview media promoted on this very request;
		// only an image is worth serving in a thumbnail's place
		let Ok(metadata) = self
			.db
			.search_file_metadata(mxc, &Dim::default(), Animate::Allowed)
			.await
		else {
			let media = self.get_stored(mxc).await?;

			return media
				.content_type
				.as_deref()
				.is_some_and(|content_type| content_type.starts_with("image/"))
				.then_some(media)
				.ok_or_else(|| err!(Request(NotFound("Media not found."))));
		};

		self.get_thumbnail_generate(mxc, &dim, animate, metadata)
			.await
	}
}

/// Answers a thumbnail row from the picture already in storage.
///
/// The response hands its body over by value, so the bytes are converted
/// rather than read in place; the conversion reclaims the storage buffer
/// whenever this holds the only handle to it.
#[implement(super::Service)]
#[tracing::instrument(name = "saved", level = "debug", skip_all)]
async fn get_thumbnail_saved(&self, data: Metadata) -> Result<Media> {
	let bytes = self.fetch_thumbnail_bytes(&data.key).await?;

	Ok(into_media(data, bytes.into()))
}

/// The stored bytes for a thumbnail row, from the first provider holding them.
///
/// Returning the shared `Bytes` spares a caller that only reads the picture
/// the `Vec` copy `Media` would force on it.
#[implement(super::Service)]
#[tracing::instrument(name = "fetch", level = "debug", skip_all)]
async fn fetch_thumbnail_bytes(&self, key: &[u8]) -> Result<Bytes> {
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
		.ok_or_else(|| err!(Request(NotFound("Media thumbnail not found."))))
}

/// Re-derive a still thumbnail from a stored animated one.
///
/// A request forbidding animation would otherwise be refused over media the
/// server already holds. Decoding the stored row answers it instead, and
/// leaves a still variant behind for the next one.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "restill", level = "debug", skip(self, data))]
async fn get_thumbnail_still(&self, mxc: &Mxc<'_>, dim: &Dim, data: Metadata) -> Result<Media> {
	// the sentinel names the original file rather than a size to re-encode at,
	// and no encoder accepts the zero dimensions it would ask for
	if dim.is_original() {
		return self.get_thumbnail_saved(data).await;
	}

	let bytes = self.fetch_thumbnail_bytes(&data.key).await?;
	let image = self.decode_still(&bytes)?;

	drop((bytes, data));

	self.store_thumbnail(mxc, dim, image).await
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
	// the sentinel names the original file rather than a size to re-encode at,
	// and no encoder accepts the zero dimensions it would ask for
	if dim.is_original() {
		return Ok(animated);
	}

	let image = self.decode_still(&animated.content)?;

	drop(animated);

	self.store_thumbnail(mxc, dim, image).await
}

#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::Service)]
#[tracing::instrument(name = "restill", level = "debug", skip_all)]
async fn get_thumbnail_still(&self, _mxc: &Mxc<'_>, _dim: &Dim, data: Metadata) -> Result<Media> {
	self.get_thumbnail_saved(data).await
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

/// Generate a thumbnail
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
	let Ok(media) = self.get_stored(mxc).await else {
		return Err!("Could not find original media.");
	};

	let frame = self.video_frame(mxc, dim, &media).await;
	let from_video = frame.is_some();

	let Ok(image) = self.decode(frame.as_deref().unwrap_or(&media.content)) else {
		// a frame the thumbnailer refuses is this video's verdict too; without
		// it the program would run again on the next request for any size
		if from_video {
			self.remember_failure(mxc);
		}

		// Couldn't parse file to generate thumbnail, send original
		return Ok(into_media(data, media.content));
	};

	drop(frame);

	// a video is never servable in place of its own thumbnail, so its frame is
	// re-encoded however small it is
	let source = Dim::new(image.width(), image.height(), None);
	if !from_video && dim.is_passthrough(&source)? && animate.accepts_picture(&media.content) {
		return Ok(into_media(data, media.content));
	}

	// nothing below reads the original, which on the video path is the whole
	// staged file, and the encode and the store must not hold it
	drop(media);

	self.store_thumbnail(mxc, dim, image).await
}

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

/// Encode a still PNG thumbnail at these dimensions and store it.
///
/// The generate path and the still-repair path share this, so a given size
/// carries one content type and one disposition whichever produced it.
#[cfg(feature = "media_thumbnail")]
#[implement(super::Service)]
#[tracing::instrument(name = "store", level = "debug", skip(self, image))]
async fn store_thumbnail(&self, mxc: &Mxc<'_>, dim: &Dim, image: DynamicImage) -> Result<Media> {
	let mut thumbnail_bytes = Vec::new();
	let thumbnail = thumbnail_generate(&image, dim)?;

	// the source raster is dead once the thumbnail exists, and neither it nor
	// the thumbnail may be held across the store below
	drop(image);

	let mut cursor = Cursor::new(&mut thumbnail_bytes);

	thumbnail
		.write_to(&mut cursor, ImageFormat::Png)
		.map_err(|error| err!(error!(?error, "Error writing PNG thumbnail.")))?;

	drop(thumbnail);

	// a generated thumbnail is a PNG rather than the uploaded file, and carries
	// the name the media repository specification asks of one whether or not the
	// original arrived with a name of its own
	let content_disposition = ContentDisposition {
		disposition_type: ContentDispositionType::Inline,
		filename: Some(THUMBNAIL_NAME.to_owned()),
	};

	// Save thumbnail in database so we don't have to generate it again next time
	let thumbnail_key =
		self.db
			.create_file_metadata(mxc, None, dim, Some(&content_disposition), Some(PNG))?;

	self.create_media_file(&thumbnail_key, &thumbnail_bytes)
		.await?;

	Ok(Media {
		content: thumbnail_bytes,
		content_type: Some(PNG.to_owned()),
		content_disposition: Some(content_disposition),
	})
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

	/// Returns true when content of this type may answer the request.
	///
	/// This reads the declared type, which whoever uploaded the picture chose,
	/// so it is only for deciding between stored rows, where the pictures
	/// themselves are not in hand. Prefer [`Self::accepts_picture`] anywhere
	/// the bytes are.
	#[inline]
	#[must_use]
	pub fn accepts_type(self, content_type: Option<&str>) -> bool {
		self.allowed() || !content_type.is_some_and(is_animated_type)
	}

	/// Returns true when this picture may answer the request.
	///
	/// The bytes decide, so a file cannot pass a request that forbade
	/// animation by declaring a content type that does not animate, and an
	/// APNG is caught despite being an `image/png` like every thumbnail.
	#[inline]
	#[must_use]
	pub fn accepts_picture(self, bytes: &[u8]) -> bool { self.allowed() || !animates(bytes) }

	/// The preference in force at these dimensions.
	///
	/// The zero dimension is the sentinel for a request too large to thumbnail,
	/// which is answered with the original file, and the original's own row is
	/// keyed there. Nothing may be withheld from it: doing so both hides the
	/// original and sends the request on to generate a zero-sized picture.
	#[must_use]
	pub fn at(self, dim: &Dim) -> Self {
		match dim.is_original() {
			| true => Self::Allowed,
			| false => self,
		}
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

/// Signature every PNG and APNG begins with.
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Chunk carrying an APNG's frame count, which a still PNG never holds.
const PNG_ANIMATION: &[u8] = b"acTL";

/// Chunk opening the pixel data, which every animation control precedes.
const PNG_DATA: &[u8] = b"IDAT";

/// Bit set in a WebP extended header when the file carries an animation.
const WEBP_ANIMATION: u8 = 0x02;

/// Bit set in a GIF descriptor when a colour table follows it.
const GIF_COLOR_TABLE: u8 = 0x80;

/// Field holding the exponent of a GIF colour table's entry count.
const GIF_TABLE_SIZE: u8 = 0x07;

/// Introducers the blocks of a GIF body begin with.
const GIF_EXTENSION: u8 = 0x21;
const GIF_IMAGE: u8 = 0x2C;
const GIF_TRAILER: u8 = 0x3B;

/// Blocks a sniff walks before it gives up on a picture.
///
/// A crafted file can chain small blocks without end, so the walk stops at a
/// bound and reports animation rather than spending anything more on it.
const SNIFF_BLOCKS: usize = 128;

/// Whether these bytes carry more than one frame.
///
/// The container header decides this rather than the declared content type,
/// which an upload takes from the client without checking it against the file.
/// Anything the walk cannot settle counts as animated, so an unreadable
/// picture is withheld rather than served against a request that forbade one.
pub(super) fn animates(bytes: &[u8]) -> bool {
	if bytes.starts_with(&PNG_MAGIC) {
		return png_animates(bytes);
	}

	if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
		return webp_animates(bytes);
	}

	if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
		return gif_animates(bytes);
	}

	// no other format the thumbnailer decodes carries a frame sequence
	false
}

/// Whether a PNG holds the control chunk that makes it an APNG.
///
/// That chunk is required to precede the pixel data, so reaching the data
/// first settles the question without any of it being read.
fn png_animates(bytes: &[u8]) -> bool {
	let Some(mut rest) = bytes.get(PNG_MAGIC.len()..) else {
		return true;
	};

	for _ in 0..SNIFF_BLOCKS {
		let Some(kind) = rest.get(4..8) else {
			return true;
		};

		if kind == PNG_ANIMATION {
			return true;
		}

		if kind == PNG_DATA {
			return false;
		}

		let Some(length) = rest
			.get(..4)
			.and_then(|field| <[u8; 4]>::try_from(field).ok())
			.map(u32::from_be_bytes)
		else {
			return true;
		};

		// the length counts the data alone, which follows a four byte length
		// and a four byte type and precedes a four byte checksum
		let Some(next) = usize::try_from(length)
			.ok()
			.and_then(|length| length.checked_add(12))
			.and_then(|skip| rest.get(skip..))
		else {
			return true;
		};

		rest = next;
	}

	true
}

/// Whether a WebP announces animation in its extended header.
///
/// Only the extended form can carry a frame sequence, so a file opening with
/// a plain lossy or lossless chunk is a still without reading any further.
fn webp_animates(bytes: &[u8]) -> bool {
	let Some(chunk) = bytes.get(12..16) else {
		return true;
	};

	if chunk != b"VP8X".as_slice() {
		return false;
	}

	// a header cut short of its flags is unreadable rather than still
	bytes
		.get(20)
		.is_none_or(|flags| flags & WEBP_ANIMATION != 0)
}

/// Whether a GIF holds more than one image descriptor.
///
/// Frame count is not written anywhere in the format, so the blocks are walked
/// until a second image is found or the trailer ends them.
fn gif_animates(bytes: &[u8]) -> bool {
	let Some(&screen) = bytes.get(10) else {
		return true;
	};

	let mut at = 13_usize;

	if screen & GIF_COLOR_TABLE != 0 {
		let Some(next) = at.checked_add(color_table_len(screen)) else {
			return true;
		};

		at = next;
	}

	let mut images = 0_usize;

	for _ in 0..SNIFF_BLOCKS {
		let Some(&block) = bytes.get(at) else {
			return true;
		};

		let Some(mut next) = at.checked_add(1) else {
			return true;
		};

		match block {
			| GIF_TRAILER => return false,
			| GIF_EXTENSION => {
				// the label, ahead of the sub-blocks every extension ends with
				let Some(after) = next.checked_add(1) else {
					return true;
				};

				next = after;
			},
			| GIF_IMAGE => {
				images = images.saturating_add(1);

				if images > 1 {
					return true;
				}

				let Some(&local) = bytes.get(next.saturating_add(8)) else {
					return true;
				};

				// the descriptor, then its own colour table, then the code size
				let Some(after) = next.checked_add(9) else {
					return true;
				};

				next = after;

				if local & GIF_COLOR_TABLE != 0 {
					let Some(after) = next.checked_add(color_table_len(local)) else {
						return true;
					};

					next = after;
				}

				let Some(after) = next.checked_add(1) else {
					return true;
				};

				next = after;
			},
			| _ => return true,
		}

		let Some(after) = skip_sub_blocks(bytes, next) else {
			return true;
		};

		at = after;
	}

	true
}

/// Bytes the colour table this descriptor announces occupies.
///
/// The size field is an exponent rather than a count, and each entry is a
/// three byte colour.
fn color_table_len(packed: u8) -> usize {
	let exponent = u32::from(packed & GIF_TABLE_SIZE).saturating_add(1);

	2_usize.saturating_pow(exponent).saturating_mul(3)
}

/// The offset past the sub-block chain beginning here, if it ends.
///
/// Each block announces its own length and a zero length closes the chain, so
/// a chain that neither closes nor runs out is abandoned at the bound.
fn skip_sub_blocks(bytes: &[u8], at: usize) -> Option<usize> {
	let mut at = at;

	for _ in 0..SNIFF_BLOCKS {
		let &size = bytes.get(at)?;
		let next = at
			.checked_add(1)?
			.checked_add(usize::from(size))?;

		if size == 0 {
			return Some(next);
		}

		at = next;
	}

	None
}

fn is_animated_type(content_type: &str) -> bool {
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

	/// Returns width, height of the thumbnail and whether it should be cropped.
	/// Returns None when the server should send the original file.
	/// Ignores the input Method.
	#[must_use]
	pub fn normalized(&self) -> Self {
		match (self.width, self.height) {
			| (0..=32, 0..=32) => Self::new(32, 32, Some(Method::Crop)),
			| (0..=96, 0..=96) => Self::new(96, 96, Some(Method::Crop)),
			| (0..=320, 0..=240) => Self::new(320, 240, Some(Method::Scale)),
			| (0..=640, 0..=480) => Self::new(640, 480, Some(Method::Scale)),
			| (0..=800, 0..=600) => Self::new(800, 600, Some(Method::Scale)),
			| _ => Self::default(),
		}
	}

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
