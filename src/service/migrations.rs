use std::{
	cmp,
	path::{Path, PathBuf},
	sync::Arc,
};

use futures::{FutureExt, StreamExt};
use ruma::{
	Mxc, MxcUri, OwnedUserId, RoomId, ServerName, UserId, events::room::member::MembershipState,
};
use tuwunel_core::{
	Err, Result, debug, debug_info, debug_warn, err, info,
	itertools::Itertools,
	matrix::PduCount,
	result::NotFound,
	utils,
	utils::{
		BoolExt, IterStream, ReadyExt,
		content_disposition::make_content_disposition,
		stream::{TryExpect, TryIgnore},
	},
	warn,
};
use tuwunel_database::{Deserialized, Map, SEP};

use crate::{Services, media};

/// The current schema version.
/// - If database is opened at greater version we reject with error. The
///   software must be updated for backward-incompatible changes.
/// - If database is opened at lesser version we apply migrations up to this.
///   Note that named-feature migrations may also be performed when opening at
///   equal or lesser version. These are expected to be backward-compatible.
pub(crate) const DATABASE_VERSION: u64 = 17;

const SERVER_NAME_KEY: &[u8] = b"server_name";

pub(crate) async fn migrations(services: &Services) -> Result {
	if !services.config.database_migrations {
		warn!("Skipping database migrations due to configuration...");
		return Ok(());
	}

	let users_count = services.users.count().await;
	if users_count == 0 {
		return fresh(services).await;
	}

	check_server_name(services).await?;
	migrate(services).await
}

/// Matrix resource ownership is based on the server name; changing it
/// requires recreating the database from scratch. The marker is stamped
/// once in fresh(); pre-marker databases are backfilled by probing for
/// any user from the configured server.
async fn check_server_name(services: &Services) -> Result {
	let server_name = &services.server.name;

	let existing = services.db["global"]
		.get(SERVER_NAME_KEY)
		.await
		.deserialized::<String>();

	match existing {
		| Err(_) => backfill_server_name(services).await,
		| Ok(existing) if existing.eq(server_name) => Ok(()),
		| Ok(existing) => Err!(Database(
			"Database belongs to {existing}; configured server name is {server_name}. Cannot \
			 reuse."
		)),
	}
}

/// Stamp the marker on a database that pre-dates SERVER_NAME_KEY by probing
/// for any user from the configured server. If none, the database belongs
/// to a different server and reuse is refused.
async fn backfill_server_name(services: &Services) -> Result {
	let server_name = &services.server.name;

	services
		.users
		.stream()
		.ready_any(|user_id| services.globals.user_is_local(user_id))
		.await
		.into_option()
		.ok_or_else(|| {
			err!(Database(
				"Database has no users from {server_name}; refusing to reuse with this \
				 server_name."
			))
		})?;

	services.db["global"].insert(SERVER_NAME_KEY, server_name.as_str());
	info!(%server_name, "Stamped server_name marker on upgraded database");

	Ok(())
}

async fn fresh(services: &Services) -> Result {
	let db = &services.db;

	services
		.globals
		.db
		.bump_database_version(DATABASE_VERSION);

	db["global"].insert(SERVER_NAME_KEY, services.server.name.as_str());
	db["global"].insert(b"feat_sha256_media", []);
	db["global"].insert(b"fix_bad_double_separator_in_state_cache", []);
	db["global"].insert(b"retroactively_fix_bad_data_from_roomuserid_joined", []);
	db["global"].insert(b"fix_referencedevents_missing_sep", []);
	db["global"].insert(b"fix_readreceiptid_readreceipt_duplicates", []);
	db["global"].insert(b"fix_hashed_sentinel_passwords", []);
	db["global"].insert(b"upgrade_legacy_mediaid_user", []);
	db["global"].insert(b"remove_remote_media_userid", []);

	// Create the admin room and server user on first run
	if services.config.create_admin_room {
		crate::admin::create_admin_room(services)
			.boxed()
			.await?;
	}

	warn!("Created new RocksDB database with version {DATABASE_VERSION}");

	Ok(())
}

/// Apply any migrations
async fn migrate(services: &Services) -> Result {
	let db = &services.db;
	let config = &services.server.config;

	let target_version = DATABASE_VERSION;
	let discovered_version = async || services.globals.db.database_version().await;

	if discovered_version().await < 13 {
		return Err!(Database(
			"Database schema version {} is no longer supported",
			discovered_version().await,
		));
	}

	// A Conduit database's colliding schema version 18 is reconciled below.
	let conduit = is_conduit_database(services).await;
	migrate_media(services, conduit).await?;

	if db["global"]
		.get(b"fix_bad_double_separator_in_state_cache")
		.await
		.is_not_found()
	{
		fix_bad_double_separator_in_state_cache(services).await?;
	}

	if db["global"]
		.get(b"retroactively_fix_bad_data_from_roomuserid_joined")
		.await
		.is_not_found()
	{
		retroactively_fix_bad_data_from_roomuserid_joined(services).await?;
	}

	if db["global"]
		.get(b"fix_referencedevents_missing_sep")
		.await
		.is_not_found()
	{
		fix_referencedevents_missing_sep(services).await?;
	}

	if db["global"]
		.get(b"fix_readreceiptid_readreceipt_duplicates")
		.await
		.is_not_found()
	{
		fix_readreceiptid_readreceipt_duplicates(services).await?;
	}

	if db["global"]
		.get(b"fix_hashed_sentinel_passwords")
		.await
		.is_not_found()
	{
		fix_hashed_sentinel_passwords(services).await?;
	}

	if db["global"]
		.get(b"upgrade_legacy_mediaid_user")
		.await
		.is_not_found()
	{
		upgrade_legacy_mediaid_user(services).await?;
	}

	if db["global"]
		.get(b"remove_remote_media_userid")
		.await
		.is_not_found()
	{
		remove_remote_media_userid(services).await?;
	}

	if discovered_version().await < target_version {
		services
			.globals
			.db
			.bump_database_version(target_version);

		info!(
			"Database: Migrated schema version from {} to {target_version}",
			discovered_version().await
		);
	} else if discovered_version().await != target_version && (config.force_migration || conduit)
	{
		services
			.globals
			.db
			.bump_database_version(target_version);

		warn!(
			"Database: Forced migration from schema version {} to {target_version}",
			discovered_version().await,
		);
	}

	assert_eq!(
		target_version,
		discovered_version().await,
		"Failed asserting local database version {} is equal to known latest tuwunel database \
		 version {target_version}",
		discovered_version().await,
	);

	if !services.config.forbidden_usernames.is_empty() {
		services
			.users
			.stream()
			.filter(|user_id| services.users.is_active_local(user_id))
			.ready_filter_map(|user_id| {
				let patterns = &services.config.forbidden_usernames;
				let matches = patterns.matches(user_id.localpart());
				let matched = matches
					.iter()
					.map(|x| &patterns.patterns()[x])
					.join(", ");

				matches
					.matched_any()
					.then_some((user_id, matched))
			})
			.ready_for_each(|(user_id, matched)| {
				warn!("User {user_id} matches forbidden username patterns: {matched:#?}");
			})
			.await;
	}

	if !services.config.forbidden_alias_names.is_empty() {
		services
			.metadata
			.iter_ids()
			.map(|room_id| {
				services
					.alias
					.local_aliases_for_room(room_id)
					.map(move |alias| (room_id, alias))
			})
			.flatten()
			.ready_filter_map(|(room_id, room_alias)| {
				let patterns = &services.config.forbidden_alias_names;
				let matches = patterns.matches(room_alias.alias());
				let matched = matches
					.iter()
					.map(|x| &patterns.patterns()[x])
					.join(", ");

				matches
					.matched_any()
					.then_some((room_id, room_alias, matched))
			})
			.ready_for_each(|(room_id, room_alias, matched)| {
				warn!(
					"Room {room_id} with alias {room_alias} matches the following forbidden \
					 room name patterns: {matched}"
				);
			})
			.boxed()
			.await;
	}

	info!("Loaded RocksDB database with schema version {DATABASE_VERSION}");

	Ok(())
}

/// Imports a Conduit database's content-addressed media into tuwunel's
/// key-addressed store; otherwise runs the key-addressed media migrations.
async fn migrate_media(services: &Services, conduit: bool) -> Result {
	let db = &services.db;
	let config = &services.server.config;

	if conduit {
		migrate_conduit_media(services).await?;
		db["global"].insert(b"feat_sha256_media", []);
		return Ok(());
	}

	if db["global"]
		.get(b"feat_sha256_media")
		.await
		.is_not_found()
	{
		media::migrations::migrate_sha256_media(services).await?;
	} else if config.media_startup_check {
		media::migrations::checkup_sha256_media(services).await?;
	}

	Ok(())
}

/// A Conduit (famedly) database opened in place. Conduit and Continuwuity both
/// stamp schema version 18, so the discriminator is Conduit's content-addressed
/// `filehash_metadata` column family, which neither tuwunel nor Continuwuity
/// has.
async fn is_conduit_database(services: &Services) -> bool {
	services.globals.db.database_version().await == 18
		&& services.db.engine.has_cf("filehash_metadata")
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

/// Lowercase hex encoding of a digest, matching the names Conduit gives its
/// content-addressed media files.
fn sha256_hex(digest: &[u8]) -> String {
	const HEX: &[u8; 16] = b"0123456789abcdef";

	let mut out = String::with_capacity(digest.len().saturating_mul(2));
	for &byte in digest {
		out.push(char::from(HEX[usize::from(byte >> 4)]));
		out.push(char::from(HEX[usize::from(byte & 0x0F)]));
	}

	out
}

/// Imports the original media files of a Conduit database, reading each entry
/// of its content-addressed `servernamemediaid_metadata` family and
/// re-uploading the file through `media.create`, which lays it out in tuwunel's
/// key-addressed store and attributes each local file to its owner. Unreadable
/// entries are logged and skipped rather than aborting the import.
async fn migrate_conduit_media(services: &Services) -> Result {
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

/// The local owner of a Conduit media entry, read from
/// `servernamemediaid_userlocalpart` (the same key, value the owner's
/// localpart). None for remote media, which has no such entry.
async fn conduit_media_owner(
	owners: Option<&Arc<Map>>,
	key: &[u8],
	server_name: &ServerName,
) -> Option<OwnedUserId> {
	let localpart = owners?.get(key).await.ok()?;

	UserId::parse_with_server_name(str::from_utf8(&localpart).ok()?, server_name).ok()
}

async fn fix_bad_double_separator_in_state_cache(services: &Services) -> Result {
	warn!("Fixing bad double separator in state_cache roomuserid_joined");

	let db = &services.db;
	let roomuserid_joined = &db["roomuserid_joined"];
	let _cork = db.cork_and_sync();

	let mut iter_count: usize = 0;
	roomuserid_joined
		.raw_stream()
		.ignore_err()
		.ready_for_each(|(key, value)| {
			let mut key = key.to_vec();
			iter_count = iter_count.saturating_add(1);
			debug_info!(%iter_count);
			let first_sep_index = key
				.iter()
				.position(|&i| i == 0xFF)
				.expect("found 0xFF delim");

			if key
				.iter()
				.get(first_sep_index..=first_sep_index.saturating_add(1))
				.copied()
				.collect_vec()
				== vec![0xFF, 0xFF]
			{
				debug_warn!("Found bad key: {key:?}");
				roomuserid_joined.remove(&key);

				key.remove(first_sep_index);
				debug_warn!("Fixed key: {key:?}");
				roomuserid_joined.insert(&key, value);
			}
		})
		.await;

	db.engine.sort()?;
	db["global"].insert(b"fix_bad_double_separator_in_state_cache", []);

	info!("Finished fixing");
	Ok(())
}

async fn retroactively_fix_bad_data_from_roomuserid_joined(services: &Services) -> Result {
	warn!("Retroactively fixing bad data from broken roomuserid_joined");

	let db = &services.db;
	let _cork = db.cork_and_sync();

	let room_ids = services
		.metadata
		.iter_ids()
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>()
		.await;

	for room_id in &room_ids {
		debug_info!("Fixing room {room_id}");

		let users_in_room: Vec<OwnedUserId> = services
			.state_cache
			.room_members(room_id)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		let joined_members = users_in_room
			.iter()
			.stream()
			.filter(|user_id| {
				services
					.state_accessor
					.get_member(room_id, user_id)
					.map(|member| {
						member.is_ok_and(|member| member.membership == MembershipState::Join)
					})
			})
			.collect::<Vec<_>>()
			.await;

		let non_joined_members = users_in_room
			.iter()
			.stream()
			.filter(|user_id| {
				services
					.state_accessor
					.get_member(room_id, user_id)
					.map(|member| {
						member.is_ok_and(|member| member.membership == MembershipState::Join)
					})
			})
			.collect::<Vec<_>>()
			.await;

		for user_id in &joined_members {
			debug_info!("User is joined, marking as joined");
			let count = services.globals.next_count();
			services
				.state_cache
				.mark_as_joined(user_id, room_id, PduCount::Normal(*count));
		}

		for user_id in &non_joined_members {
			debug_info!("User is left or banned, marking as left");
			let count = services.globals.next_count();
			services
				.state_cache
				.mark_as_left(user_id, room_id, PduCount::Normal(*count));
		}
	}

	for room_id in &room_ids {
		debug_info!(
			"Updating joined count for room {room_id} to fix servers in room after correcting \
			 membership states"
		);

		services
			.state_cache
			.update_joined_count(room_id)
			.await;
	}

	db.engine.sort()?;
	db["global"].insert(b"retroactively_fix_bad_data_from_roomuserid_joined", []);

	info!("Finished fixing");
	Ok(())
}

async fn fix_referencedevents_missing_sep(services: &Services) -> Result {
	warn!("Fixing missing record separator between room_id and event_id in referencedevents");

	let db = &services.db;
	let cork = db.cork_and_sync();

	let referencedevents = db["referencedevents"].clone();

	let totals: (usize, usize) = (0, 0);
	let (total, fixed) = referencedevents
		.raw_stream()
		.expect_ok()
		.enumerate()
		.ready_fold(totals, |mut a, (i, (key, val))| {
			debug_assert!(val.is_empty(), "expected no value");

			let has_sep = key.contains(&SEP);

			if !has_sep {
				let key_str = std::str::from_utf8(key).expect("key not utf-8");
				let room_id_len = key_str.find('$').expect("missing '$' in key");
				let (room_id, event_id) = key_str.split_at(room_id_len);
				debug!(?a, "fixing {room_id}, {event_id}");

				let new_key = (room_id, event_id);
				referencedevents.put_raw(new_key, val);
				referencedevents.remove(key);
			}

			a.0 = cmp::max(i, a.0);
			a.1 = a.1.saturating_add((!has_sep).into());
			a
		})
		.await;

	drop(cork);
	info!(?total, ?fixed, "Fixed missing record separators in 'referencedevents'.");

	db["global"].insert(b"fix_referencedevents_missing_sep", []);
	db.engine.sort()
}

async fn fix_readreceiptid_readreceipt_duplicates(services: &Services) -> Result {
	use ruma::identifiers_validation::ID_MAX_BYTES;
	use tuwunel_core::arrayvec::ArrayString;

	type ArrayId = ArrayString<ID_MAX_BYTES>;
	type Key<'a> = (&'a RoomId, u64, &'a UserId);

	warn!("Fixing undeleted entries in readreceiptid_readreceipt...");

	let db = &services.db;
	let cork = db.cork_and_sync();
	let readreceiptid_readreceipt = db["readreceiptid_readreceipt"].clone();

	let mut cur_room: Option<ArrayId> = None;
	let mut cur_user: Option<ArrayId> = None;
	let (mut total, mut fixed): (usize, usize) = (0, 0);
	readreceiptid_readreceipt
		.keys()
		.expect_ok()
		.ready_for_each(|key: Key<'_>| {
			let (room_id, _, user_id) = key;
			let last_room = cur_room.replace(
				room_id
					.as_str()
					.try_into()
					.expect("invalid room_id in database"),
			);

			let last_user = cur_user.replace(
				user_id
					.as_str()
					.try_into()
					.expect("invalid user_id in database"),
			);

			let is_dup = cur_room == last_room && cur_user == last_user;
			if is_dup {
				readreceiptid_readreceipt.del(key);
			}

			fixed = fixed.saturating_add(is_dup.into());
			total = total.saturating_add(1);
		})
		.await;

	drop(cork);
	info!(?total, ?fixed, "Fixed undeleted entries in readreceiptid_readreceipt.");

	db["global"].insert(b"fix_readreceiptid_readreceipt_duplicates", []);
	db.engine.sort()
}

async fn fix_hashed_sentinel_passwords(services: &Services) -> Result {
	use tuwunel_core::utils::hash::verify_password;

	const PASSWORD_SENTINEL: &str = "*";

	if services.config.identity_provider.is_empty() {
		debug!("Skipping sentinel password migration since no SSO IdP configured.");
		return Ok(());
	}

	let db = &services.db;
	let cork = db.cork_and_sync();
	let userid_password = db["userid_password"].clone();
	let hashed_sentinel = utils::hash::password(PASSWORD_SENTINEL).map_err(|e| {
		err!("Could not apply migration: failed to hash sentinel password: {e:?}")
	})?;

	warn!(
		"Fixing occurrences of password-hash {hashed_sentinel:?} generated from \
		 {PASSWORD_SENTINEL:?}"
	);

	let (checked, good, bad) = userid_password
		.stream()
		.expect_ok()
		.ready_fold(
			(0, 0, 0),
			|(mut checked, mut good, mut bad): (usize, usize, usize),
			 (key, val): (&str, &str)| {
				let good_sentinel = val == PASSWORD_SENTINEL;
				let bad_sentinel = !val.is_empty()
					&& !good_sentinel
					&& verify_password(PASSWORD_SENTINEL, val).is_ok();

				checked = checked.saturating_add(usize::from(true));
				good = good.saturating_add(usize::from(good_sentinel));
				bad = bad.saturating_add(usize::from(bad_sentinel));

				if bad_sentinel {
					userid_password.insert(key, PASSWORD_SENTINEL);
				}

				(checked, good, bad)
			},
		)
		.await;

	drop(cork);
	info!(?checked, ?good, ?bad, "Fixed any occurrences of hashed sentinel passwords");

	db["global"].insert(b"fix_hashed_sentinel_passwords", []);
	db.engine.sort()
}

async fn upgrade_legacy_mediaid_user(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let mediaid_user = db["mediaid_user"].clone();

	warn!("Upgrading legacy mediaid_user keys to composite (mxc, user_id) layout");

	let (checked, upgraded, removed_invalid) = mediaid_user
		.raw_stream()
		.ignore_err()
		.ready_fold(
			(0_usize, 0_usize, 0_usize),
			|(mut checked, mut upgraded, mut removed_invalid), (raw_key, raw_val)| {
				checked = checked.saturating_add(1);

				let has_sep = raw_key.contains(&SEP);
				let user_id = str::from_utf8(raw_val)
					.ok()
					.and_then(|s| <&UserId>::try_from(s).ok());

				match (has_sep, user_id) {
					| (true, _) => {},
					| (false, None) => {
						warn!(
							?raw_key,
							?raw_val,
							"Legacy entry has unparsable user_id, removing"
						);

						mediaid_user.remove(raw_key);
						removed_invalid = removed_invalid.saturating_add(1);
					},
					| (false, Some(user_id)) => {
						let mut new_key = raw_key.to_vec();
						new_key.push(SEP);
						new_key.extend_from_slice(user_id.as_bytes());

						mediaid_user.put_raw(new_key, user_id.as_str());
						mediaid_user.remove(raw_key);

						upgraded = upgraded.saturating_add(1);
					},
				}

				(checked, upgraded, removed_invalid)
			},
		)
		.await;

	drop(cork);
	info!(
		%checked,
		%upgraded,
		%removed_invalid,
		"Upgraded legacy mediaid_user keys"
	);

	db["global"].insert(b"upgrade_legacy_mediaid_user", []);
	db.engine.sort()
}

async fn remove_remote_media_userid(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let mediaid_user = db["mediaid_user"].clone();

	warn!("Removing stored user id for remote media");

	let (checked, removed_remote, removed_invalid) = mediaid_user
		.keys()
		.expect_ok()
		.ready_fold(
			(0, 0, 0),
			|(mut checked, mut removed_remote, mut removed_invalid): (usize, usize, usize),
			 (mxc_uri, user_id): (&MxcUri, &UserId)| {
				checked = checked.saturating_add(1);

				let Ok(mxc) = mxc_uri.parts() else {
					warn!(?mxc_uri, "Invalid MXC URL, removing it");

					mediaid_user.del((mxc_uri, user_id));

					removed_invalid = removed_invalid.saturating_add(1);

					return (checked, removed_remote, removed_invalid);
				};

				if !services.globals.server_is_ours(mxc.server_name) {
					mediaid_user.del((mxc_uri, user_id));

					removed_remote = removed_remote.saturating_add(1);

					return (checked, removed_remote, removed_invalid);
				}

				(checked, removed_remote, removed_invalid)
			},
		)
		.await;

	drop(cork);
	info!(
		%checked,
		%removed_remote,
		%removed_invalid,
		"Removed stored user id for remote media"
	);

	db["global"].insert(b"remove_remote_media_userid", []);
	db.engine.sort()
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
