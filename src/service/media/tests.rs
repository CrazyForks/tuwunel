#![cfg(test)]

use ruma::media::Method;

use super::Dim;

fn scale(width: u32, height: u32) -> Dim { Dim::new(width, height, Some(Method::Scale)) }
fn crop(width: u32, height: u32) -> Dim { Dim::new(width, height, Some(Method::Crop)) }
fn source(width: u32, height: u32) -> Dim { Dim::new(width, height, None) }

/// The source already carries the requested dimensions, so scaling is a no-op
/// and re-encoding would only discard the original's format and metadata.
#[test]
fn passthrough_when_scale_request_matches_source() {
	assert!(
		scale(800, 600)
			.is_passthrough(&source(800, 600))
			.unwrap()
	);
}

/// `scaled` covers the requested box rather than fitting inside it, so a source
/// already meeting the requested height cannot shrink: the generated thumbnail
/// would be the source itself.
#[test]
fn passthrough_when_one_side_already_meets_request() {
	assert!(
		scale(800, 600)
			.is_passthrough(&source(1000, 600))
			.unwrap()
	);
}

/// A genuinely larger source must still be thumbnailed.
#[test]
fn generates_when_source_is_larger_in_both_dimensions() {
	assert!(
		!scale(800, 600)
			.is_passthrough(&source(4000, 3000))
			.unwrap()
	);
}

/// Crop reaches the requested size by cropping, so a source that is larger in
/// only one dimension must still be generated. Widening the upscale guard to
/// `>=` would wrongly pass this through.
#[test]
fn generates_when_crop_source_is_larger_in_one_dimension() {
	assert!(
		!crop(96, 96)
			.is_passthrough(&source(500, 96))
			.unwrap()
	);
}

/// Servers must not upscale; a source smaller than the request is served as-is.
#[test]
fn passthrough_when_request_exceeds_source() {
	assert!(
		crop(96, 96)
			.is_passthrough(&source(50, 50))
			.unwrap()
	);
	assert!(
		scale(800, 600)
			.is_passthrough(&source(1000, 400))
			.unwrap()
	);
}

/// Crop at exactly the source size reproduces the source.
#[test]
fn passthrough_when_crop_request_matches_source() {
	assert!(
		crop(96, 96)
			.is_passthrough(&source(96, 96))
			.unwrap()
	);
}

mod animate {
	use super::{
		super::{Animate, Dim, thumbnail::ANIMATED_TYPES},
		scale,
	};

	/// MSC2705 gives `animated` three states but only two behaviors.
	///
	/// An explicit `animated=true` is the only one that may be answered with
	/// animation; `false` and an absent parameter alike forbid it.
	#[test]
	fn three_parameter_states_map_to_two_behaviors() {
		assert_eq!(Animate::from(Some(true)), Animate::Allowed);
		assert_eq!(Animate::from(Some(false)), Animate::Never);
		assert_eq!(Animate::from(None), Animate::Never);
	}

	/// A request for a still picture must not be answered with any type an
	/// animation can arrive in.
	///
	/// This is MSC2705's only MUST NOT. The list is the constant itself, so a
	/// type added there is covered on sight.
	#[test]
	fn still_request_withholds_every_animated_type() {
		for content_type in ANIMATED_TYPES {
			assert!(
				!Animate::Never.accepts(Some(content_type)),
				"{content_type} must not answer a still request"
			);
		}
	}

	/// Withholding is confined to that family.
	///
	/// The still types a thumbnail is generated as must keep answering every
	/// request, or the rule would withhold the very variant it asks for.
	#[test]
	fn still_request_accepts_still_types() {
		for content_type in
			[Some("image/png"), Some("image/jpeg"), Some("image/png; charset=binary"), None]
		{
			assert!(Animate::Never.accepts(content_type), "{content_type:?} must be servable");
		}
	}

	/// A request that asked for animation is not owed one.
	///
	/// The MSC phrases animation as a SHOULD, so such a request accepts
	/// whichever variant is on hand rather than refusing a still.
	#[test]
	fn animated_request_accepts_anything() {
		for content_type in [Some("image/gif"), Some("image/png"), None] {
			assert!(Animate::Allowed.accepts(content_type), "{content_type:?} must be servable");
		}
	}

	/// Withholding survives the shapes a content type arrives in.
	///
	/// These reach the classifier from remote servers and stored keys alike,
	/// so the match tolerates the parameters and casing a peer may attach.
	#[test]
	fn withholding_survives_parameters_and_casing() {
		assert!(!Animate::Never.accepts(Some("image/GIF")));
		assert!(!Animate::Never.accepts(Some("image/gif; charset=binary")));
		assert!(!Animate::Never.accepts(Some("Image/WebP ")));
	}

	/// A type merely spelled like an animated one is a different type.
	///
	/// The match is on the whole essence rather than a prefix, so neither a
	/// longer subtype nor a different top-level type is withheld.
	#[test]
	fn withholding_does_not_reach_beyond_the_family() {
		assert!(Animate::Never.accepts(Some("image/gifted")));
		assert!(Animate::Never.accepts(Some("video/webp")));
	}

	/// A request too large to thumbnail normalizes to the sentinel.
	///
	/// That sentinel stands for the original file, and is also the key the
	/// original itself is stored under.
	#[test]
	fn oversized_request_normalizes_to_the_original_sentinel() {
		assert!(scale(1200, 900).normalized().is_original());
		assert!(!scale(800, 600).normalized().is_original());
		assert!(!scale(32, 32).normalized().is_original());
	}

	/// Nothing may be withheld at that sentinel.
	///
	/// Withholding there hides the original from a request for a still and
	/// sends it on to generate at zero dimensions, which no encoder accepts.
	#[test]
	fn the_original_sentinel_withholds_nothing() {
		assert_eq!(Animate::Never.at(&Dim::default()), Animate::Allowed);
		assert_eq!(Animate::Never.at(&scale(1200, 900).normalized()), Animate::Allowed);
		assert_eq!(Animate::Never.at(&scale(800, 600).normalized()), Animate::Never);
	}
}

#[cfg(feature = "media_thumbnail")]
mod generate {
	use image::{DynamicImage, RgbImage};

	use super::{super::thumbnail::thumbnail_generate, crop, scale};

	fn blank(width: u32, height: u32) -> DynamicImage {
		DynamicImage::ImageRgb8(RgbImage::new(width, height))
	}

	/// Servers must not upscale under any circumstance. A video's frame reaches
	/// generation without the passthrough guard that spares an image, so the
	/// crop path has to refuse to enlarge on its own.
	#[test]
	fn crop_never_upscales() {
		let thumbnail = thumbnail_generate(&blank(50, 50), &crop(96, 96)).unwrap();

		assert_eq!((thumbnail.width(), thumbnail.height()), (50, 50));
	}

	/// A source larger in one dimension only still must not grow in the other.
	#[test]
	fn crop_never_upscales_one_dimension() {
		let thumbnail = thumbnail_generate(&blank(500, 50), &crop(96, 96)).unwrap();

		assert_eq!((thumbnail.width(), thumbnail.height()), (96, 50));
	}

	/// A crop request inside the source is still honoured exactly.
	#[test]
	fn crop_within_the_source_is_exact() {
		let thumbnail = thumbnail_generate(&blank(500, 300), &crop(96, 96)).unwrap();

		assert_eq!((thumbnail.width(), thumbnail.height()), (96, 96));
	}

	/// The scale path clamps through `Dim::scaled`; pin it so the two branches
	/// cannot drift apart.
	#[test]
	fn scale_never_upscales() {
		let thumbnail = thumbnail_generate(&blank(50, 40), &scale(800, 600)).unwrap();

		assert!(thumbnail.width() <= 50 && thumbnail.height() <= 40);
	}
}

#[cfg(feature = "media_thumbnail")]
mod video {
	use std::{borrow::Cow, path::Path};

	use super::super::video::substitute;

	/// The staged path and the requested size reach the program only through
	/// token substitution, and a token may appear more than once in one
	/// argument.
	#[test]
	fn substitutes_every_token_in_an_argument() {
		let path = Path::new("/tmp/tuwunel-video-Ahk3");
		let arg = substitute("{input}:{width}x{height}:{width}", path, "320", "240");

		assert_eq!(arg, "/tmp/tuwunel-video-Ahk3:320x240:320");
	}

	/// An argument carrying no token is the program's own flag, and must reach
	/// it byte for byte without being copied to do so.
	#[test]
	fn borrows_an_argument_without_a_token() {
		let path = Path::new("/tmp/tuwunel-video-Ahk3");
		let arg = substitute("-frames:v", path, "320", "240");

		assert_eq!(arg, "-frames:v");
		assert!(matches!(arg, Cow::Borrowed(_)), "an untouched argument was copied");
	}
}

#[cfg(all(unix, feature = "media_thumbnail"))]
mod program {
	use std::{env::temp_dir, fs::remove_file, time::Duration};

	use tokio::time::{Instant, sleep, timeout};
	use tuwunel_core::utils::random_string;

	use super::super::video::run;

	const SHELL: &str = "/bin/sh";

	const LIMIT: u64 = 4096;

	/// Every program under test exits at once; the deadline is present only to
	/// keep a hung one from hanging the suite.
	fn deadline() -> Instant { after(Duration::from_secs(30)) }

	fn after(duration: Duration) -> Instant {
		Instant::now()
			.checked_add(duration)
			.expect("a deadline within the epoch")
	}

	/// The frame arrives on standard output, not through a file the program is
	/// told to write.
	#[tokio::test]
	async fn collects_the_frame_from_standard_output() {
		let args = ["-c", "printf frame"];
		let frame = run(SHELL, args, LIMIT, deadline())
			.await
			.expect("a program writing output produces a frame");

		assert_eq!(frame.as_slice(), b"frame");
	}

	/// A program can exit zero having decoded nothing, so an empty output is a
	/// failure rather than a zero-length thumbnail.
	#[tokio::test]
	async fn rejects_a_program_that_writes_no_frame() {
		let args = ["-c", "exit 0"];

		run(SHELL, args, LIMIT, deadline())
			.await
			.expect_err("a program writing nothing produces no frame");
	}

	/// A misconfigured command is diagnosed from the program's own standard
	/// error, which is the only account of why it failed.
	#[tokio::test]
	async fn reports_the_diagnostic_of_a_failing_program() {
		let args = ["-c", "echo Unknown encoder >&2; exit 1"];
		let error = run(SHELL, args, LIMIT, deadline())
			.await
			.expect_err("a non-zero exit is a failure")
			.to_string();

		assert!(error.contains("Unknown encoder"), "{error}");
	}

	/// A program overrunning the deadline is killed along with whatever it
	/// spawned. The wrapper exits at once and leaves the descendant holding
	/// the pipe, which is the shape that orphans work: waiting on the direct
	/// child alone reports the program finished while its decoder runs on.
	#[tokio::test]
	async fn kills_the_group_of_a_wrapper_that_exits() {
		let marker = temp_dir().join(format!("tuwunel-group-{}", random_string(16)));
		let script = format!("(sleep 1; touch {}) &", marker.display());
		let args = ["-c", script.as_str()];

		run(SHELL, args, LIMIT, after(Duration::from_millis(200)))
			.await
			.expect_err("a program past its deadline fails");

		// outlive the descendant's own delay, so that its absence means it was
		// killed rather than merely still sleeping
		sleep(Duration::from_millis(1500)).await;

		let survived = marker.exists();
		remove_file(&marker).ok();

		assert!(!survived, "a descendant outlived the killed group");
	}

	/// Shutdown and a disconnected client both drop the request rather than
	/// expire a deadline, so the group has to die on the dropped future too.
	#[tokio::test]
	async fn kills_the_group_of_a_cancelled_program() {
		let marker = temp_dir().join(format!("tuwunel-cancel-{}", random_string(16)));
		let script = format!("(sleep 1; touch {}) &", marker.display());
		let args = ["-c", script.as_str()];

		// boxed so that dropping the binding drops the future itself, which a
		// stack pin would not
		let mut running = Box::pin(run(SHELL, args, LIMIT, deadline()));

		timeout(Duration::from_millis(200), &mut running)
			.await
			.expect_err("the program should still be running");

		drop(running);
		sleep(Duration::from_millis(1500)).await;

		let survived = marker.exists();
		remove_file(&marker).ok();

		assert!(!survived, "a descendant outlived the cancelled request");
	}

	/// A frame past the limit is refused outright: serving the prefix would
	/// hand the thumbnailer a truncation to fail on, reporting a decode error
	/// for what is really an oversized frame.
	#[tokio::test]
	async fn refuses_a_frame_past_the_limit() {
		let args = ["-c", "printf frame"];
		let error = run(SHELL, args, 3, deadline())
			.await
			.expect_err("five bytes is past a three byte limit")
			.to_string();

		assert!(error.contains("past 3 bytes"), "{error}");
	}

	/// A frame filling the limit exactly is not mistaken for one past it.
	#[tokio::test]
	async fn accepts_a_frame_filling_the_limit() {
		let args = ["-c", "printf frame"];
		let frame = run(SHELL, args, 5, deadline())
			.await
			.expect("five bytes fits a five byte limit");

		assert_eq!(frame.as_slice(), b"frame");
	}
}

#[tokio::test]
#[cfg(disable)] //TODO: fixme
async fn long_file_names_works() {
	use std::path::PathBuf;

	use base64::{Engine as _, engine::general_purpose};

	use super::*;

	struct MockedKVDatabase;

	impl Data for MockedKVDatabase {
		fn create_file_metadata(
			&self,
			_sender_user: Option<&str>,
			mxc: String,
			width: u32,
			height: u32,
			content_disposition: Option<&str>,
			content_type: Option<&str>,
		) -> Result<Vec<u8>> {
			// copied from src/database/key_value/media.rs
			let mut key = mxc.as_bytes().to_vec();
			key.push(0xFF);
			key.extend_from_slice(&width.to_be_bytes());
			key.extend_from_slice(&height.to_be_bytes());
			key.push(0xFF);
			key.extend_from_slice(
				content_disposition
					.as_ref()
					.map(|f| f.as_bytes())
					.unwrap_or_default(),
			);
			key.push(0xFF);
			key.extend_from_slice(
				content_type
					.as_ref()
					.map(|c| c.as_bytes())
					.unwrap_or_default(),
			);

			Ok(key)
		}

		fn delete_file_mxc(&self, _mxc: String) -> Result { todo!() }

		fn search_mxc_metadata_prefix(&self, _mxc: String) -> Result<Vec<Vec<u8>>> { todo!() }

		fn get_all_media_keys(&self) -> Vec<Vec<u8>> { todo!() }

		fn search_file_metadata(
			&self,
			_mxc: String,
			_width: u32,
			_height: u32,
		) -> Result<(Option<String>, Option<String>, Vec<u8>)> {
			todo!()
		}

		fn remove_url_preview(&self, _url: &str) -> Result { todo!() }

		fn set_url_preview(
			&self,
			_url: &str,
			_data: &UrlPreviewData,
			_timestamp: std::time::Duration,
		) -> Result {
			todo!()
		}

		fn get_url_preview(&self, _url: &str) -> Option<UrlPreviewData> { todo!() }
	}

	let db: Arc<MockedKVDatabase> = Arc::new(MockedKVDatabase);
	let mxc = "mxc://example.com/ascERGshawAWawugaAcauga".to_owned();
	let width = 100;
	let height = 100;
	let content_disposition = "attachment; filename=\"this is a very long file name with spaces \
	                           and special characters like äöüß and even emoji like 🦀.png\"";
	let content_type = "image/png";
	let key = db
		.create_file_metadata(
			None,
			mxc,
			width,
			height,
			Some(content_disposition),
			Some(content_type),
		)
		.unwrap();
	let mut r = PathBuf::from("/tmp/media");
	// r.push(base64::encode_config(key, base64::URL_SAFE_NO_PAD));
	// use the sha256 hash of the key as the file name instead of the key itself
	// this is because the base64 encoded key can be longer than 255 characters.
	r.push(general_purpose::URL_SAFE_NO_PAD.encode(<sha2::Sha256 as sha2::Digest>::digest(key)));
	// Check that the file path is not longer than 255 characters
	// (255 is the maximum length of a file path on most file systems)
	assert!(
		r.to_str().unwrap().len() <= 255,
		"File path is too long: {}",
		r.to_str().unwrap().len()
	);
}
