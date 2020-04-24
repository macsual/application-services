/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// The "outgoing" part of syncing - building the payloads to upload and
// managing the sync state of the local DB.

use interrupt_support::Interruptee;
use rusqlite::{Connection, Row, Transaction};
use sql_support::ConnExt;
use sync15_traits::ServerTimestamp;
use sync_guid::Guid as SyncGuid;

use crate::error::*;

use super::ServerPayload;

// This is the "state" for outgoing items - it's meta-data for an item that we
// use to update the local DB after we POST outgoing records.
#[derive(Debug)]
pub struct OutgoingStateHolder {
    ext_id: String,
    change_counter: i32,
}

// A simple holder for the payload that needs to be sent to the servers, and
// the above item meta-data.
#[derive(Debug)]
pub struct OutgoingInfo {
    pub state: OutgoingStateHolder,
    pub payload: ServerPayload,
}

impl OutgoingInfo {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        let guid = row
            .get::<_, Option<SyncGuid>>("guid")?
            .unwrap_or_else(SyncGuid::random);
        let ext_id: String = row.get("ext_id")?;
        let raw_data: Option<String> = row.get("data")?;
        let (data, deleted) = if raw_data.is_some() {
            (raw_data, false)
        } else {
            (None, true)
        };
        Ok(OutgoingInfo {
            state: OutgoingStateHolder {
                ext_id: ext_id.clone(),
                change_counter: row.get("sync_change_counter")?,
            },
            payload: ServerPayload {
                ext_id,
                guid,
                data,
                deleted,
                last_modified: ServerTimestamp(0),
            },
        })
    }
}

/// Gets info about what should be uploaded. Returns a vec of the payload which
/// should be uploaded, plus meta-data for those items which should be held
/// until the upload is complete, then passed back to record_uploaded.
pub fn get_outgoing<S: ?Sized + Interruptee>(
    conn: &Connection,
    _signal: &S,
) -> Result<Vec<OutgoingInfo>> {
    let sql = "SELECT l.ext_id, l.data, l.sync_change_counter, m.guid
               FROM storage_sync_data l
               LEFT JOIN storage_sync_mirror m ON m.ext_id = l.ext_id
               WHERE sync_change_counter > 0";
    let elts = conn
        .conn()
        .query_rows_and_then_named(sql, &[], OutgoingInfo::from_row)?;
    log::debug!("get_outgoing found {} items", elts.len());
    Ok(elts)
}

/// Record the fact that an item was uploaded. This updates the state of the
/// local DB to reflect the state of the server we just updated.
pub fn record_uploaded<S: ?Sized + Interruptee>(
    tx: &Transaction<'_>,
    items: &[OutgoingInfo],
    signal: &S,
) -> Result<()> {
    log::debug!(
        "record_uploaded recording that {} items were uploaded",
        items.len()
    );

    let sql = "
        UPDATE storage_sync_data SET
            sync_change_counter = (sync_change_counter - :old_counter)
        WHERE ext_id = :ext_id;";
    for item in items.iter() {
        signal.err_if_interrupted()?;
        log::trace!(
            "recording ext_id='{}' - {:?} was uploaded",
            item.state.ext_id,
            item.payload
        );
        tx.execute_named(
            sql,
            rusqlite::named_params! {
                ":old_counter": item.state.change_counter,
                ":ext_id": item.state.ext_id,
            },
        )?;
    }

    // Copy staging into the mirror, then kill staging, and local tombstones.
    tx.execute_batch(
        "
        REPLACE INTO storage_sync_mirror (guid, ext_id, server_modified, data)
        SELECT guid, ext_id, server_modified, data FROM temp.storage_sync_staging;

        DELETE FROM temp.storage_sync_staging;

        DELETE from storage_sync_data WHERE data IS NULL;",
    )?;

    // And the stuff that was uploaded should be placed in the mirror.
    // XXX - server_modified is wrong here - do we even need it in the schema?
    let sql = "
        REPLACE INTO storage_sync_mirror (guid, ext_id, server_modified, data)
        VALUES (:guid, :ext_id, :server_modified, :data);
    ";
    for item in items.iter() {
        signal.err_if_interrupted()?;
        tx.execute_named(
            sql,
            rusqlite::named_params! {
                ":guid": item.payload.guid,
                ":ext_id": item.state.ext_id,
                ":server_modified": item.payload.last_modified.0, // XXX - wrong!
                ":data": item.payload.data,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::new_syncable_mem_db;
    use super::*;
    use interrupt_support::NeverInterrupts;

    #[test]
    fn test_simple() -> Result<()> {
        let db = new_syncable_mem_db();
        let mut conn = db.writer.lock().unwrap();
        let tx = conn.transaction()?;

        tx.execute_batch(
            r#"
            INSERT INTO storage_sync_data (ext_id, data, sync_change_counter)
            VALUES
                ('ext_no_changes', '{"foo":"bar"}', 0),
                ('ext_with_changes', '{"foo":"bar"}', 1);

        "#,
        )?;

        let changes = get_outgoing(&tx, &NeverInterrupts)?;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].state.ext_id, "ext_with_changes".to_string());

        record_uploaded(&tx, &changes, &NeverInterrupts)?;

        let counter: i32 = tx.conn().query_one(
            "SELECT sync_change_counter FROM storage_sync_data WHERE ext_id = 'ext_with_changes'",
        )?;
        assert_eq!(counter, 0);
        Ok(())
    }
}
