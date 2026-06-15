use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	sync::Arc,
};

use futures::StreamExt;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, Mxc, OwnedRoomId, OwnedUserId, RoomId,
	ServerName, UserId,
};
use tuwunel_core::{
	Err, Result, debug_warn, err, info, utils,
	utils::{ReadyExt, content_disposition::make_content_disposition, stream::TryIgnore},
	warn,
};
use tuwunel_database::{Map, SEP};

use crate::Services;

/// A Conduit (famedly) database opened in place. Conduit and Continuwuity both
/// stamp schema version 18, so the discriminator is Conduit's content-addressed
/// `filehash_metadata` column family, which neither tuwunel nor Continuwuity
/// has.
pub(super) async fn is_conduit_database(services: &Services) -> bool {
	services.globals.db.database_version().await == 18
		&& services.db.engine.has_cf("filehash_metadata")
}

/// Imports the original media files of a Conduit database, re-uploading each
/// `servernamemediaid_metadata` entry through `media.create`. Unreadable
/// entries are logged and skipped rather than aborting the import.
pub(super) async fn migrate_conduit_media(services: &Services) -> Result {
	let db = &services.db;
	let config = &services.server.config;

	let Some(metadata) = db.open_cf("servernamemediaid_metadata")? else {
		warn!("Conduit database has no media metadata; nothing to import.");
		return Ok(());
	};

	let owners = db.open_cf("servernamemediaid_userlocalpart")?;
	let owners = owners.as_ref();

	let media_dir = config
		.conduit_source_media_path
		.clone()
		.unwrap_or_else(|| config.database_path.join("media"));

	let depth = config.conduit_media_directory_depth;
	let length = config.conduit_media_directory_length;

	warn!("Importing Conduit media originals into tuwunel's key-addressed store...");

	let cork = db.cork_and_sync();
	let (imported, skipped) = metadata
		.raw_stream()
		.ignore_err()
		.fold((0_usize, 0_usize), async |(imported, skipped), (key, value)| {
			match import_conduit_original(services, owners, &media_dir, depth, length, key, value)
				.await
			{
				| Ok(()) => (imported.saturating_add(1), skipped),
				| Err(e) => {
					debug_warn!(error = %e, "skipping unimportable Conduit media entry");
					(imported, skipped.saturating_add(1))
				},
			}
		})
		.await;

	drop(cork);

	if skipped > 0 {
		warn!(%imported, %skipped, "Imported Conduit media originals; some files were skipped");
	} else {
		info!(%imported, "Imported Conduit media originals");
	}

	Ok(())
}

/// Imports one `servernamemediaid_metadata` entry: the key is
/// `servername 0xff media_id`, the value is `sha256(32) | filename | 0xff |
/// content_type`. The file lives at the digest's content-addressed path.
async fn import_conduit_original(
	services: &Services,
	owners: Option<&Arc<Map>>,
	media_dir: &Path,
	depth: u8,
	length: u8,
	key: &[u8],
	value: &[u8],
) -> Result {
	let Some(sep) = key.iter().position(|&byte| byte == SEP) else {
		return Err!(Database("Conduit media key has no server-name separator"));
	};
	let server_name = <&ServerName>::try_from(str::from_utf8(&key[..sep])?)
		.map_err(|_| err!(Database("Conduit media key has an invalid server name")))?;

	let media_id = str::from_utf8(&key[sep.saturating_add(1)..])?;

	let (sha256, rest) = value
		.split_at_checked(32)
		.ok_or_else(|| err!(Database("Conduit media value shorter than a SHA-256 digest")))?;
	let Some(sep) = rest.iter().position(|&byte| byte == SEP) else {
		return Err!(Database("Conduit media value has no content-type separator"));
	};
	let filename = str::from_utf8(&rest[..sep])?;
	let content_type = str::from_utf8(&rest[sep.saturating_add(1)..])?;
	let filename = (!filename.is_empty()).then_some(filename);
	let content_type = (!content_type.is_empty()).then_some(content_type);

	let path = conduit_media_path(media_dir, depth, length, &sha256_hex(sha256));
	let file = tokio::fs::read(&path)
		.await
		.map_err(|e| err!(Database("reading Conduit media file {path:?}: {e}")))?;

	let content_disposition = make_content_disposition(None, content_type, filename);
	let owner = conduit_media_owner(owners, key, server_name).await;
	let mxc = Mxc { server_name, media_id };

	services
		.media
		.create(&mxc, owner.as_deref(), Some(&content_disposition), content_type, &file)
		.await
}

/// The local owner of a Conduit media entry from
/// `servernamemediaid_userlocalpart`; `None` for remote media, which has no
/// such entry.
async fn conduit_media_owner(
	owners: Option<&Arc<Map>>,
	key: &[u8],
	server_name: &ServerName,
) -> Option<OwnedUserId> {
	let localpart = owners?.get(key).await.ok()?;

	UserId::parse_with_server_name(str::from_utf8(&localpart).ok()?, server_name).ok()
}

/// Reconstructs the on-disk path of a Conduit content-addressed media file from
/// the lowercase SHA-256 hex digest naming it, matching Conduit's
/// `split_media_path`: a `depth` of zero is a flat directory, otherwise the
/// digest is sharded into `depth` segments of `length` characters then the
/// remainder. `config::check` bounds `depth * length` so `length` never
/// overruns the digest here.
fn conduit_media_path(media_dir: &Path, depth: u8, length: u8, sha256_hex: &str) -> PathBuf {
	let mut path = media_dir.to_path_buf();
	if depth == 0 {
		path.push(sha256_hex);
		return path;
	}

	let mut rest = sha256_hex;
	for _ in 0..depth {
		let Some((segment, next)) = rest.split_at_checked(length.into()) else {
			break;
		};

		path.push(segment);
		rest = next;
	}

	path.push(rest);
	path
}

/// Lowercase hex digest, matching the names Conduit gives its media files.
fn sha256_hex(digest: &[u8]) -> String {
	const HEX: &[u8; 16] = b"0123456789abcdef";

	let mut out = String::with_capacity(digest.len().saturating_mul(2));
	for &byte in digest {
		out.push(char::from(HEX[usize::from(byte >> 4)]));
		out.push(char::from(HEX[usize::from(byte & 0x0F)]));
	}

	out
}

/// Injects `room_id` into the stored Conduit PDUs that lack it. Only a room v12
/// (`hydra`) create event omits the field, deriving its room from the event's
/// own id per MSC4291; every other event already stores `room_id` and the pass
/// skips it. tuwunel's `PduEvent` requires the field, so those create events
/// fail to deserialize without this. Scans both the `pduid_pdu` timeline (room
/// from the key's leading short room id) and `eventid_outlierpdu` (room from
/// the create event's own id, which is the outlier key).
pub(super) async fn migrate_conduit_pdus(services: &Services) -> Result {
	let db = &services.db;

	// shortroomid -> room_id, inverted once so resolving each timeline PDU's
	// room is a lookup rather than a scan of roomid_shortroomid.
	let rooms: BTreeMap<u64, OwnedRoomId> = db["roomid_shortroomid"]
		.stream()
		.ignore_err()
		.map(|(room_id, short): (&RoomId, u64)| (short, room_id.to_owned()))
		.collect()
		.await;

	warn!("Reconciling Conduit PDUs into tuwunel's format...");
	let cork = db.cork_and_sync();

	let pduid_pdu = &db["pduid_pdu"];
	let timeline = pduid_pdu
		.raw_stream()
		.ignore_err()
		.ready_fold((0_usize, 0_usize), |acc, (key, value)| {
			tally(acc, inject_room_id(pduid_pdu, key, value, |_| pduid_room(&rooms, key)))
		})
		.await;

	let outlier = &db["eventid_outlierpdu"];
	let outliers = outlier
		.raw_stream()
		.ignore_err()
		.ready_fold((0_usize, 0_usize), |acc, (key, value)| {
			tally(acc, inject_room_id(outlier, key, value, |pdu| outlier_room(key, pdu)))
		})
		.await;

	drop(cork);

	let fixed = timeline.0.saturating_add(outliers.0);
	let skipped = timeline.1.saturating_add(outliers.1);
	if skipped > 0 {
		warn!(%fixed, %skipped, "Reconciled Conduit PDUs; some were skipped");
	} else {
		info!(%fixed, "Reconciled Conduit PDUs");
	}

	Ok(())
}

fn tally((fixed, skipped): (usize, usize), result: Result<bool>) -> (usize, usize) {
	match result {
		| Ok(true) => (fixed.saturating_add(1), skipped),
		| Ok(false) => (fixed, skipped),
		| Err(e) => {
			debug_warn!(error = %e, "skipping unreconcilable Conduit PDU");
			(fixed, skipped.saturating_add(1))
		},
	}
}

/// Injects `room_id` into one PDU value that lacks it, sourcing the room from
/// `resolve`. Returns whether the value was rewritten; `false` means it already
/// carried a `room_id`.
fn inject_room_id(
	map: &Arc<Map>,
	key: &[u8],
	value: &[u8],
	resolve: impl FnOnce(&CanonicalJsonObject) -> Result<OwnedRoomId>,
) -> Result<bool> {
	let mut pdu: CanonicalJsonObject = serde_json::from_slice(value)
		.map_err(|e| err!(Database("Conduit PDU is not canonical JSON: {e}")))?;

	if pdu.contains_key("room_id") {
		return Ok(false);
	}

	let room_id = resolve(&pdu)?;
	pdu.insert("room_id".into(), CanonicalJsonValue::String(room_id.as_str().into()));

	let bytes = serde_json::to_vec(&pdu)
		.map_err(|e| err!(Database("re-serializing reconciled Conduit PDU: {e}")))?;

	map.insert(key, bytes);

	Ok(true)
}

/// The room of a `pduid_pdu` entry, from the short room id leading its key.
fn pduid_room(rooms: &BTreeMap<u64, OwnedRoomId>, key: &[u8]) -> Result<OwnedRoomId> {
	let short = key
		.get(..8)
		.ok_or_else(|| err!(Database("Conduit pduid is shorter than a short room id")))?;

	rooms
		.get(&utils::u64_from_u8(short))
		.cloned()
		.ok_or_else(|| err!(Database("Conduit pduid short room id maps to no room")))
}

/// The room of an `eventid_outlierpdu` entry that lacks `room_id`. Only a v12
/// create event omits it, and its room id derives from the create event's own
/// id, which is this outlier's key.
fn outlier_room(key: &[u8], pdu: &CanonicalJsonObject) -> Result<OwnedRoomId> {
	let is_create = matches!(
		pdu.get("type"),
		Some(CanonicalJsonValue::String(kind)) if kind == "m.room.create"
	);

	if !is_create {
		return Err!(Database("Conduit outlier lacks room_id and is not a create event"));
	}

	let event_id = <&EventId>::try_from(str::from_utf8(key)?)
		.map_err(|_| err!(Database("Conduit outlier key is not a valid event id")))?;

	RoomId::new_v2(event_id.localpart())
		.map_err(|e| err!(Database("deriving room id from create event id: {e}")))
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{conduit_media_path, sha256_hex};

	#[test]
	fn conduit_media_path_deep_matches_conduit_default() {
		// Conduit default Deep { length: 2, depth: 2 }: two 2-char segments of the
		// 64-char digest, then the remaining 60 characters.
		let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
		let path = conduit_media_path(Path::new("/db/media"), 2, 2, hex);

		assert_eq!(
			path,
			Path::new(
				"/db/media/01/23/456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
			)
		);
	}

	#[test]
	fn conduit_media_path_flat_is_unsharded() {
		let path = conduit_media_path(Path::new("/db/media"), 0, 2, "abcdef");

		assert_eq!(path, Path::new("/db/media/abcdef"));
	}

	#[test]
	fn sha256_hex_encodes_lowercase_padded() {
		assert_eq!(sha256_hex(&[0x00, 0x0F, 0xFF, 0xA5]), "000fffa5");
	}
}
