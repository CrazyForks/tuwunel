mod keys;
mod keys_room;
mod keys_session;
mod version;
mod version_id;

use futures::{FutureExt, future::try_join};
use http::StatusCode;
use ruma::{
	CanonicalJsonValue, UInt, UserId,
	api::error::{ErrorKind, WrongRoomKeysVersionErrorData},
	serde::Raw,
};
use serde::Deserialize;
use tuwunel_core::{Error, Result};
use tuwunel_service::Services;

pub(crate) use self::{
	keys::{add_backup_keys_route, delete_backup_keys_route, get_backup_keys_route},
	keys_room::{
		add_backup_keys_for_room_route, delete_backup_keys_for_room_route,
		get_backup_keys_for_room_route,
	},
	keys_session::{
		add_backup_keys_for_session_route, delete_backup_keys_for_session_route,
		get_backup_keys_for_session_route,
	},
	version::{create_backup_version_route, get_latest_backup_info_route},
	version_id::{
		delete_backup_version_route, get_backup_info_route, update_backup_version_route,
	},
};

/// Overrides ruma's internal `AlgorithmWithData` shape required by the GET
/// `/room_keys/version[/{version}]` response serializer. Validating against
/// this will not raise a serialization error (HTTP 500) when responding.
#[derive(Deserialize)]
#[expect(unused)]
struct AlgorithmShape {
	algorithm: Raw<CanonicalJsonValue>,
	auth_data: Raw<CanonicalJsonValue>,
}

pub(super) fn validate_algorithm_shape<T>(raw: &Raw<T>) -> Result {
	raw.deserialize_as_unchecked::<AlgorithmShape>()
		.map_err(Into::into)
		.map(drop)
}

pub(super) async fn get_count_etag(
	services: &Services,
	sender_user: &UserId,
	version: &str,
) -> Result<(UInt, String)> {
	let count = services
		.key_backups
		.count_keys(sender_user, version)
		.map(|count| count.try_into().map_err(Error::from));

	let etag = services
		.key_backups
		.get_etag(sender_user, version)
		.map(|result| result.map(|etag| etag.to_string()));

	try_join(count, etag).await
}

pub(super) async fn check_backup_exists(
	services: &Services,
	sender_user: &UserId,
	version: &str,
) -> Result {
	let algorithm = services
		.key_backups
		.get_backup(sender_user, version)
		.map(|result| result.map(drop));

	let etag = services
		.key_backups
		.get_etag(sender_user, version)
		.map(|result| result.map(drop));

	try_join(algorithm, etag).await.map(drop)
}

pub(super) async fn check_backup_version(
	services: &Services,
	sender_user: &UserId,
	version: &str,
) -> Result {
	let current_version = services
		.key_backups
		.get_latest_backup_version(sender_user)
		.await?;

	if current_version == version {
		return Ok(());
	}

	let data = WrongRoomKeysVersionErrorData::new(current_version);
	let kind = ErrorKind::WrongRoomKeysVersion(data);
	let error = Error::Request(
		kind,
		"You may only manipulate the most recently created version of the backup.".into(),
		StatusCode::BAD_REQUEST,
	);

	Err(error)
}
