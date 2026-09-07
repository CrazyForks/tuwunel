use axum::extract::State;
use futures::{StreamExt, TryStreamExt};
use ruma::api::client::backup::{
	add_backup_keys_for_room, delete_backup_keys_for_room, get_backup_keys_for_room,
};
use tuwunel_core::{Result, utils::stream::IterStream};

use super::{check_backup_exists, check_backup_version, get_count_etag};
use crate::Ruma;

/// # `PUT /_matrix/client/r0/room_keys/keys/{roomId}`
///
/// Add the received backup keys to the database.
///
/// - Only manipulating the most recently created version of the backup is
///   allowed
/// - Adds the keys to the backup
/// - Returns the new number of keys in this backup and the etag
pub(crate) async fn add_backup_keys_for_room_route(
	State(services): State<crate::State>,
	body: Ruma<add_backup_keys_for_room::v3::Request>,
) -> Result<add_backup_keys_for_room::v3::Response> {
	check_backup_version(&services, body.sender_user(), &body.version).await?;

	body.sessions
		.iter()
		.stream()
		.map(Ok)
		.try_for_each(|(session_id, key_data)| {
			services.key_backups.add_key(
				body.sender_user(),
				&body.version,
				&body.room_id,
				session_id,
				key_data,
			)
		})
		.await?;

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(add_backup_keys_for_room::v3::Response { count, etag })
}

/// # `GET /_matrix/client/r0/room_keys/keys/{roomId}`
///
/// Retrieves all keys from the backup for a given room.
pub(crate) async fn get_backup_keys_for_room_route(
	State(services): State<crate::State>,
	body: Ruma<get_backup_keys_for_room::v3::Request>,
) -> Result<get_backup_keys_for_room::v3::Response> {
	let sessions = services
		.key_backups
		.get_room(body.sender_user(), &body.version, &body.room_id)
		.await;

	Ok(get_backup_keys_for_room::v3::Response { sessions })
}

/// # `DELETE /_matrix/client/r0/room_keys/keys/{roomId}`
///
/// Delete the keys from the backup for a given room.
pub(crate) async fn delete_backup_keys_for_room_route(
	State(services): State<crate::State>,
	body: Ruma<delete_backup_keys_for_room::v3::Request>,
) -> Result<delete_backup_keys_for_room::v3::Response> {
	check_backup_exists(&services, body.sender_user(), &body.version).await?;

	services
		.key_backups
		.delete_room_keys(body.sender_user(), &body.version, &body.room_id)
		.await?;

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(delete_backup_keys_for_room::v3::Response { count, etag })
}
