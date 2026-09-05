//! Container animation sniffing
//!
//! A picture names its own format in its first bytes and carries its frame
//! sequence, if it has one, at a place each format fixes. Reading those is what
//! lets a request for a still be honored against a file whose declared content
//! type says otherwise, or says nothing useful, as `image/png` does for an
//! APNG.
//!
//! Every walk here fails closed, so an unreadable, truncated or unrecognized
//! picture answers as animated and the cost of a wrong answer is a needless
//! re-encode rather than a violation.

use tuwunel_core::utils::math::checked_ops;

use super::{APNG, GIF, WEBP};

/// Leading signatures of the three animating containers.
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const RIFF_MAGIC: &[u8] = b"RIFF";
const WEBP_MAGIC: &[u8] = b"WEBP";
const GIF_MAGIC: [&[u8]; 2] = [b"GIF87a", b"GIF89a"];

/// Chunk names that settle a PNG.
///
/// The animation control is required to precede the pixel data, so meeting
/// the data first settles the question without reading any of it.
const PNG_ANIMATION: &[u8] = b"acTL";
const PNG_DATA: &[u8] = b"IDAT";

/// Chunk names a WebP can open with, and the animation flag's own bit.
///
/// Only the extended form can hold a frame sequence, so a file opening with a
/// plain lossy or lossless chunk is a still without reading further.
const WEBP_EXTENDED: &[u8] = b"VP8X";
const WEBP_ANIMATION: u8 = 0x02;

/// Leading byte of each block in a GIF body.
///
/// A body is a flat sequence of these, ending at the trailer, and animation
/// is the presence of a second image rather than anything the format states.
const GIF_EXTENSION: u8 = 0x21;
const GIF_IMAGE: u8 = 0x2C;
const GIF_TRAILER: u8 = 0x3B;

/// GIF colour-table flag and size bits.
///
/// The flag says whether one follows and the size field holds the exponent of
/// its entry count, each entry being a three byte colour.
const GIF_COLOR_TABLE: u8 = 0x80;
const GIF_TABLE_SIZE: u8 = 0x07;

/// Bytes the signature and the logical screen descriptor occupy together.
///
/// A global colour table, when one is announced, follows them, and the block
/// sequence follows that.
const GIF_HEADER_LEN: usize = 13;

/// Whether these bytes carry more than one frame.
///
/// The container header decides this rather than the declared content type,
/// which an upload takes from the client without checking it against the file.
pub(in super::super) fn animates(bytes: &[u8]) -> bool { animated_type(bytes).is_some() }

/// The content type an animating picture ought to be stored under.
///
/// A picture that does not animate answers `None`, its declared type being no
/// worse than anything read from its header, and so does one in any other
/// format, none of which carries a frame sequence.
pub(in super::super) fn animated_type(bytes: &[u8]) -> Option<&'static str> {
	let is_gif = GIF_MAGIC
		.iter()
		.any(|magic| bytes.starts_with(magic));

	match bytes {
		| _ if bytes.starts_with(PNG_MAGIC) => png_animates(bytes).then_some(APNG),
		| _ if bytes.starts_with(RIFF_MAGIC) && bytes.get(8..12) == Some(WEBP_MAGIC) =>
			webp_animates(bytes).then_some(WEBP),
		| _ if is_gif => gif_animates(bytes).then_some(GIF),
		| _ => None,
	}
}

/// Whether a PNG holds the control chunk that makes it an APNG.
///
/// Each hop clears a whole chunk, so the walk advances by at least its twelve
/// byte frame every time and ends when a hop lands past the picture.
fn png_animates(bytes: &[u8]) -> bool {
	let Some(mut rest) = bytes.get(PNG_MAGIC.len()..) else {
		return true;
	};

	loop {
		let Some(kind) = rest.get(4..8) else {
			return true;
		};

		if kind == PNG_ANIMATION {
			return true;
		}

		if kind == PNG_DATA {
			return false;
		}

		// the length counts the data alone, which follows a four byte length
		// and a four byte type and precedes a four byte checksum
		let Some(next) = rest
			.get(..4)
			.and_then(|field| field.try_into().ok())
			.map(u32::from_be_bytes)
			.and_then(|length| usize::try_from(length).ok())
			.and_then(|length| length.checked_add(12))
			.and_then(|skip| rest.get(skip..))
		else {
			return true;
		};

		rest = next;
	}
}

/// Whether a WebP announces animation in its extended header.
///
/// The chunk name and the flags sit at fixed offsets, so this reads two fields
/// and never walks, and a header cut short of either is unreadable.
fn webp_animates(bytes: &[u8]) -> bool {
	bytes.get(12..16).is_none_or(|chunk| {
		chunk == WEBP_EXTENDED
			&& bytes
				.get(20)
				.is_none_or(|flags| flags & WEBP_ANIMATION != 0)
	})
}

/// Whether a GIF holds more than one image descriptor.
///
/// Frame count is written nowhere in the format, so the blocks are walked
/// until a second image is found or the trailer ends them. Every block clears
/// at least its own introducer and a terminating sub-block, so the offset
/// rises on every pass and a walk that runs off the picture ends.
fn gif_animates(bytes: &[u8]) -> bool {
	let Some(&screen) = bytes.get(10) else {
		return true;
	};

	let global_table = (screen & GIF_COLOR_TABLE != 0)
		.then(|| color_table_len(screen))
		.unwrap_or_default();

	let Some(mut at) = global_table.checked_add(GIF_HEADER_LEN) else {
		return true;
	};

	let mut seen = false;

	loop {
		let Some(&block) = bytes.get(at) else {
			return true;
		};

		let image = block == GIF_IMAGE;

		if image && seen {
			return true;
		}

		seen |= image;

		let next = match block {
			| GIF_TRAILER => return false,
			| GIF_EXTENSION => at.checked_add(2),
			| GIF_IMAGE => image_data_start(bytes, at),
			| _ => return true,
		};

		let Some(after) = next.and_then(|next| skip_sub_blocks(bytes, next)) else {
			return true;
		};

		at = after;
	}
}

/// Size in bytes of the colour table a descriptor announces.
///
/// The size field is an exponent rather than a count, and each entry is a
/// three byte colour.
fn color_table_len(packed: u8) -> usize {
	let exponent = u32::from(packed & GIF_TABLE_SIZE).saturating_add(1);

	2_usize.saturating_pow(exponent).saturating_mul(3)
}

/// Offset of an image descriptor's sub-block chain.
///
/// The descriptor is a fixed nine bytes carrying the flag for a colour table
/// of its own, and one further byte holds the code size the data opens with.
fn image_data_start(bytes: &[u8], at: usize) -> Option<usize> {
	let descriptor = at.checked_add(1)?;
	let packed = *bytes.get(descriptor.checked_add(8)?)?;
	let local_table = (packed & GIF_COLOR_TABLE != 0)
		.then(|| color_table_len(packed))
		.unwrap_or_default();

	checked_ops!(descriptor + 9 + local_table + 1)
}

/// Offset past the sub-block chain starting here, when it ends.
///
/// Each block announces its own length and a zero length closes the chain, so
/// every pass clears at least the length byte and a chain that neither closes
/// nor runs out of picture cannot occur.
fn skip_sub_blocks(bytes: &[u8], mut at: usize) -> Option<usize> {
	loop {
		let size = usize::from(*bytes.get(at)?);

		at = checked_ops!(at + 1 + size)?;

		if size == 0 {
			return Some(at);
		}
	}
}
