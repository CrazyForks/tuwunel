use axum::extract::State;
use ruma::api::client::backup::{
	add_backup_keys_for_session, delete_backup_keys_for_session, get_backup_keys_for_session,
};
use tuwunel_core::{Result, err};

use super::{check_backup_exists, check_backup_version, get_count_etag};
use crate::Ruma;

/// # `PUT /_matrix/client/r0/room_keys/keys/{roomId}/{sessionId}`
///
/// Add the received backup key to the database.
///
/// - Only manipulating the most recently created version of the backup is
///   allowed
/// - Adds the keys to the backup
/// - Returns the new number of keys in this backup and the etag
pub(crate) async fn add_backup_keys_for_session_route(
	State(services): State<crate::State>,
	body: Ruma<add_backup_keys_for_session::v3::Request>,
) -> Result<add_backup_keys_for_session::v3::Response> {
	check_backup_version(&services, body.sender_user(), &body.version).await?;

	services
		.key_backups
		.add_key(
			body.sender_user(),
			&body.version,
			&body.room_id,
			&body.session_id,
			&body.session_data,
		)
		.await?;

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(add_backup_keys_for_session::v3::Response { count, etag })
}

/// # `GET /_matrix/client/r0/room_keys/keys/{roomId}/{sessionId}`
///
/// Retrieves a key from the backup.
pub(crate) async fn get_backup_keys_for_session_route(
	State(services): State<crate::State>,
	body: Ruma<get_backup_keys_for_session::v3::Request>,
) -> Result<get_backup_keys_for_session::v3::Response> {
	let key_data = services
		.key_backups
		.get_session(body.sender_user(), &body.version, &body.room_id, &body.session_id)
		.await
		.map_err(|error| {
			if error.is_not_found() {
				err!(Request(NotFound(debug_error!(
					"Backup key not found for this user's session."
				))))
			} else {
				error
			}
		})?;

	Ok(get_backup_keys_for_session::v3::Response { key_data })
}

/// # `DELETE /_matrix/client/r0/room_keys/keys/{roomId}/{sessionId}`
///
/// Delete a key from the backup.
pub(crate) async fn delete_backup_keys_for_session_route(
	State(services): State<crate::State>,
	body: Ruma<delete_backup_keys_for_session::v3::Request>,
) -> Result<delete_backup_keys_for_session::v3::Response> {
	check_backup_exists(&services, body.sender_user(), &body.version).await?;

	services.key_backups.delete_room_key(
		body.sender_user(),
		&body.version,
		&body.room_id,
		&body.session_id,
	);

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(delete_backup_keys_for_session::v3::Response { count, etag })
}
