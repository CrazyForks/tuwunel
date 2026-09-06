#![cfg(test)]
#![cfg(feature = "media_thumbnail")]

//! What a peer's mislabelled thumbnail is cached under.
//!
//! A server federating with itself only ever meets its own honest labels, so
//! the snapshot harness cannot reach the case this covers: a peer answering
//! with a picture whose bytes contradict the type it declares. That is how an
//! APNG ordinarily arrives, and the row a fetch leaves behind is all a later
//! lookup has to go on, so the claim is corrected rather than stored.
//!
//! Two writes absorb a peer's picture and each is covered here, because the
//! peer answers the two federation media endpoints in the shapes they each
//! specify: the authenticated one in `multipart/mixed`, the legacy one as a
//! bare body under a content type header.

use std::{
	env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
	time::Duration,
};

use axum::{Router, http::header::CONTENT_TYPE, response::IntoResponse, routing::any};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use tokio::spawn;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
// the endpoint is deprecated, and that it still writes a row is the case here
#[expect(deprecated)]
use tuwunel_core::ruma::api::client::media::get_content_thumbnail::v3::Request as LegacyRequest;
use tuwunel_core::{
	Err, Result, err,
	ruma::{Mxc, ServerName, UInt, UserId, media::Method},
};
use tuwunel_service::{
	Services,
	media::{Animate, Dim},
};

/// A one-by-one GIF carrying two frames.
///
/// The second image descriptor is what settles the walk on an animation, and
/// so what the peer's declared type contradicts.
const GIF: &[u8] = &[
	0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
	0x00, 0xFF, 0xFF, 0xFF, 0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00,
	0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x44, 0x00, 0x21, 0xF9, 0x04, 0x01,
	0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02,
	0x01, 0x44, 0x00, 0x3B,
];

/// What the peer claims its picture to be.
///
/// A real APNG arrives under exactly this type, honestly, which is what makes
/// a declared type worth contradicting rather than trusting.
const DECLARED: &str = "image/png";

/// What the walk settles on, and so what the stored row has to carry.
///
/// The two content types differing is the whole result, so this is the half
/// the assertion is written against.
const SETTLED: &str = "image/gif";

/// Separator for the peer's `multipart/mixed` answer.
///
/// The value is arbitrary except that it must not occur in the picture it
/// separates, which no run of letters can.
const BOUNDARY: &str = "peerstubmultipartboundary";

/// One media per endpoint, since the row a fetch stores answers later lookups.
///
/// Sharing one would let whichever case ran first satisfy the other from cache,
/// leaving the second endpoint's own write untested.
const MEDIA_ID: &str = "peermislabelledthumbnailmediaid0";

const LEGACY_MEDIA_ID: &str = "peermislabelledlegacythumbnailid";

const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";

const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";

const TIMEOUT: Duration = Duration::from_secs(10);

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// A peer's animation declared a still is cached under the type its bytes name.
///
/// The answer this request receives is the peer's own, declared type included,
/// so the correction is only observable in the row left behind. Reading that
/// row back is therefore the assertion, and the two content types differing is
/// the whole result.
#[test]
fn a_peers_mislabelled_thumbnail_is_cached_under_its_container() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let address = format!("127.0.0.1:{}", listener.local_addr()?.port());
	let peer = ServerName::parse(&address).map_err(|e| err!("peer server name: {e}"))?;

	// tokio refuses to adopt a blocking socket, and the panic it raises reaches
	// the caller only as a peer that never answered
	listener.set_nonblocking(true)?;

	let root = var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
	let db_path = DatabasePath(
		PathBuf::from(root).join(format!("tuwunel-media-peer-relabel-{}", process_id())),
	);

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let certificate = manifest.join(CERTIFICATE);
	let private_key = manifest.join(PRIVATE_KEY);

	let args = [
		format!("database_path={:?}", db_path.0),
		"allow_invalid_tls_certificates=true".to_owned(),
		"ip_range_denylist=[]".to_owned(),
		"federation_loopback=true".to_owned(),
		"log=\"error\"".to_owned(),
	]
	.into_iter()
	.fold(Args::default_test(&["fresh", "cleanup"]), Args::with_option);

	// the option form of this one is refused, so the field is the only way in
	let args = Args { maintenance: true, ..args };

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let stub = spawn(serve_peer(listener, certificate, private_key));
		let outcome = exercise(&services, &peer).await;
		let outcome = outcome.and(exercise_legacy(&services, &peer).await);

		// a stub that never started reaches the caller only as a peer that did
		// not answer, so its own error is reported in place of that symptom
		let outcome = match stub.is_finished() {
			| true => stub
				.await
				.unwrap_or_else(|error| Err(err!("the peer stub panicked: {error}")))
				.and(outcome),
			| false => {
				stub.abort();
				outcome
			},
		};

		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

/// Answers every federation request with the same mislabelled picture.
///
/// The peer is reachable only because its name is the literal address it
/// listens on, which federation resolves without a lookup. Nothing here
/// verifies the request's signature, since what is under test is what the
/// answer is cached as rather than how it was asked for.
async fn serve_peer(listener: TcpListener, certificate: PathBuf, private_key: PathBuf) -> Result {
	let config = RustlsConfig::from_pem_file(certificate, private_key).await?;
	let app = Router::new()
		.route("/_matrix/federation/{*rest}", any(authenticated))
		.fallback(any(legacy));

	from_tcp_rustls(listener, config)?
		.serve(app.into_make_service())
		.await?;

	Ok(())
}

/// The `multipart/mixed` body an authenticated media answer carries.
///
/// The parts and their separators are the shape the client half of the API
/// writes, so a body assembled any other way would fail to parse before
/// reaching anything this covers.
async fn authenticated() -> impl IntoResponse {
	let head = format!(
		"\r\n--{BOUNDARY}\r\ncontent-type: \
		 application/json\r\n\r\n{{}}\r\n--{BOUNDARY}\r\ncontent-type: {DECLARED}\r\n\r\n"
	);

	let body = [
		head.as_bytes(),
		GIF,
		b"\r\n--".as_slice(),
		BOUNDARY.as_bytes(),
		b"--".as_slice(),
	]
	.concat();

	([(CONTENT_TYPE, format!("multipart/mixed; boundary={BOUNDARY}"))], body)
}

/// The bare body a legacy media answer carries.
///
/// The unauthenticated endpoint predates the multipart envelope, so the
/// picture is the whole body and the type it is declared under is a header.
async fn legacy() -> impl IntoResponse { ([(CONTENT_TYPE, DECLARED)], GIF) }

async fn exercise(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc { server_name: peer, media_id: MEDIA_ID };

	let dim = Dim::new(96, 96, None);
	let user = UserId::parse_with_server_name("peerfixture", services.globals.server_name())?;

	let fetched = services
		.media
		.get_or_fetch_thumbnail(&mxc, &dim, Animate::Allowed, TIMEOUT, &user)
		.await?;

	if fetched.content != GIF {
		return Err!("the peer's own picture did not reach the caller");
	}

	// the answer is relayed as the peer sent it, so a correction here would be
	// a different change than the one under test
	let relayed = fetched
		.content_type
		.as_deref()
		.unwrap_or_default();

	if relayed != DECLARED {
		return Err!("the relayed answer was labelled {relayed}, expected {DECLARED}");
	}

	let stored = services
		.media
		.get_stored_thumbnail(&mxc, &dim, Animate::Allowed)
		.await?;

	let cached = stored.content_type.as_deref().unwrap_or_default();

	if cached != SETTLED {
		return Err!("the cached row was stored as {cached}, expected {SETTLED}");
	}

	Ok(())
}

/// The same claim over the legacy endpoint, which is a second write.
///
/// This one answers the caller with the row it just stored rather than with
/// the peer's own bytes, so the declared type is not relayed and the stored
/// one is the only thing to assert.
#[expect(deprecated)]
async fn exercise_legacy(services: &Services, peer: &ServerName) -> Result {
	let mxc = Mxc {
		server_name: peer,
		media_id: LEGACY_MEDIA_ID,
	};

	let dim = Dim::new(96, 96, None);
	let request = LegacyRequest {
		server_name: peer.to_owned(),
		media_id: LEGACY_MEDIA_ID.to_owned(),
		method: Some(Method::Scale),
		width: UInt::from(96_u32),
		height: UInt::from(96_u32),
		allow_remote: true,
		timeout_ms: TIMEOUT,
		allow_redirect: false,
		animated: Some(true),
	};

	services
		.media
		.fetch_remote_thumbnail_legacy(&request)
		.await?;

	let stored = services
		.media
		.get_stored_thumbnail(&mxc, &dim, Animate::Allowed)
		.await?;

	let cached = stored.content_type.as_deref().unwrap_or_default();

	if cached != SETTLED {
		return Err!("the legacy row was stored as {cached}, expected {SETTLED}");
	}

	Ok(())
}
