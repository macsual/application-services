/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// The "incoming" part of syncing - handling the incoming rows, staging them,
// working out a plan for them, updating the local data and mirror, etc.

use interrupt_support::Interruptee;
use rusqlite::{types::ToSql, Connection, Row, Transaction};
use serde_json;
use sql_support::ConnExt;
use sync_guid::Guid as SyncGuid;

use crate::error::*;

use super::{merge, JsonMap, ServerPayload};

// This module deals exclusively with the Map inside a JsonValue::Object().
// This helper reads such a Map from a SQL row, ignoring anything which is
// either invalid JSON or a different JSON type.
fn json_map_from_row(row: &Row<'_>, col: &str) -> Result<Option<JsonMap>> {
    let s = row.get::<_, Option<String>>(col)?;
    Ok(match s {
        None => None,
        Some(s) => match serde_json::from_str(&s) {
            Ok(serde_json::Value::Object(m)) => Some(m),
            _ => {
                // We don't want invalid json or wrong types to kill syncing.
                // It should be impossible as we never write anything which
                // could cause it, but we can't really log the bad data as there
                // might be PII. Logging just a message without any additional
                // clues is going to be unhelpfully noisy, so, silently None.
                // XXX - Maybe record telemetry?
                None
            }
        },
    })
}

/// The first thing we do with incoming items is to "stage" them in a temp table.
/// The actual processing is done via this table.
pub fn stage_incoming<S: ?Sized + Interruptee>(
    tx: &Transaction<'_>,
    incoming_bsos: Vec<ServerPayload>,
    signal: &S,
) -> Result<()> {
    sql_support::each_sized_chunk(
        &incoming_bsos,
        // We bind 3 params per chunk.
        sql_support::default_max_variable_number() / 3,
        |chunk, _| -> Result<()> {
            let sql = format!(
                "INSERT OR REPLACE INTO temp.storage_sync_staging
                (guid, ext_id, data)
                VALUES {}",
                sql_support::repeat_multi_values(chunk.len(), 3)
            );
            let mut params = Vec::with_capacity(chunk.len() * 3);
            for bso in chunk {
                signal.err_if_interrupted()?;
                params.push(&bso.guid as &dyn ToSql);
                params.push(&bso.ext_id);
                params.push(&bso.data);
            }
            tx.execute(&sql, &params)?;
            Ok(())
        },
    )?;
    Ok(())
}

/// Details about an incoming item.
#[derive(Debug, PartialEq)]
pub struct IncomingItem {
    guid: SyncGuid,
    ext_id: String,
}

/// The "state" we find ourselves in when considering an incoming/staging
/// record. This "state" is the input to calculating the IncomingAction.
#[derive(Debug, PartialEq)]
pub enum IncomingState {
    IncomingOnly {
        incoming: Option<JsonMap>,
    },
    HasLocal {
        incoming: Option<JsonMap>,
        local: Option<JsonMap>,
    },
    NotLocal {
        incoming: Option<JsonMap>,
        mirror: Option<JsonMap>,
    },
    Everywhere {
        incoming: Option<JsonMap>,
        mirror: Option<JsonMap>,
        local: Option<JsonMap>,
    },
}

/// Get the items we need to process from the staging table. Return details about
/// the item and the state of that item, ready for processing.
pub fn get_incoming(conn: &Connection) -> Result<Vec<(IncomingItem, IncomingState)>> {
    let sql = "
        SELECT
            s.guid as guid,
            m.guid IS NOT NULL as m_exists,
            l.ext_id IS NOT NULL as l_exists,
            s.ext_id,
            s.data as s_data, m.data as m_data, l.data as l_data,
            l.sync_change_counter
        FROM temp.storage_sync_staging s
        LEFT JOIN storage_sync_mirror m ON m.guid = s.guid
        LEFT JOIN storage_sync_data l on l.ext_id = s.ext_id;";

    fn from_row(row: &Row<'_>) -> Result<(IncomingItem, IncomingState)> {
        let guid = row.get("guid")?;
        let ext_id = row.get("ext_id")?;
        let incoming = json_map_from_row(row, "s_data")?;

        let mirror_exists = row.get("m_exists")?;
        let local_exists = row.get("l_exists")?;

        let state = match (local_exists, mirror_exists) {
            (false, false) => IncomingState::IncomingOnly { incoming },
            (true, false) => IncomingState::HasLocal {
                incoming,
                local: json_map_from_row(row, "l_data")?,
            },
            (false, true) => IncomingState::NotLocal {
                incoming,
                mirror: json_map_from_row(row, "m_data")?,
            },
            (true, true) => IncomingState::Everywhere {
                incoming,
                mirror: json_map_from_row(row, "m_data")?,
                local: json_map_from_row(row, "l_data")?,
            },
        };
        Ok((IncomingItem { guid, ext_id }, state))
    }

    Ok(conn.conn().query_rows_and_then_named(sql, &[], from_row)?)
}

/// This is the set of actions we know how to take *locally* for incoming
/// records. Which one depends on the IncomingState.
#[derive(Debug, PartialEq)]
pub enum IncomingAction {
    // Something's wrong with this entry - probably JSON?
    // (but we seem to be getting away without this for now)
    //Invalid { reason: String },
    /// We should locally delete the data for this record
    DeleteLocally,
    /// We will take the remote.
    TakeRemote { data: JsonMap },
    /// We merged this data - this is what we came up with.
    Merge { data: JsonMap },
    /// Entry exists locally and it's the same as the incoming record.
    Same,
}

/// Takes the state of an item and returns the action we should take for it.
pub fn plan_incoming(s: IncomingState) -> IncomingAction {
    match s {
        IncomingState::Everywhere {
            incoming,
            local,
            mirror,
        } => {
            // All records exist - but do they all have data?
            match (incoming, local, mirror) {
                (Some(id), Some(ld), Some(md)) => {
                    // all records have data - 3-way merge.
                    merge(id, ld, Some(md))
                }
                (Some(id), Some(ld), None) => {
                    // No parent, so first time seeing this remotely - 2-way merge
                    merge(id, ld, None)
                }
                (Some(id), None, _) => {
                    // Local Incoming data, removed locally. Server wins.
                    IncomingAction::TakeRemote { data: id }
                }
                (None, _, _) => {
                    // Deleted remotely. Server wins.
                    // XXX - WRONG - we want to 3 way merge here still!
                    // Eg, final key removed remotely, different key added
                    // locally, the new key should still be added.
                    IncomingAction::DeleteLocally
                }
            }
        }
        IncomingState::HasLocal { incoming, local } => {
            // So we have a local record and an incoming/staging record, but *not* a
            // mirror record. This means some other device has synced this for
            // the first time and we are yet to do the same.
            match (incoming, local) {
                (Some(id), Some(ld)) => {
                    // This means the extension exists locally and remotely
                    // but this is the first time we've synced it. That's no problem, it's
                    // just a 2-way merge...
                    merge(id, ld, None)
                }
                (Some(_), None) => {
                    // We've data locally, but there's an incoming deletion.
                    // Remote wins.
                    IncomingAction::DeleteLocally
                }
                (None, Some(data)) => {
                    // No data locally, but some is incoming - take it.
                    IncomingAction::TakeRemote { data }
                }
                (None, None) => {
                    // Nothing anywhere - odd, but OK.
                    IncomingAction::Same
                }
            }
        }
        IncomingState::NotLocal { incoming, .. } => {
            // No local data but there's mirror and an incoming record.
            // This means a local deletion is being replaced by, or just re-doing
            // the incoming record.
            match incoming {
                Some(data) => IncomingAction::TakeRemote { data },
                None => IncomingAction::Same,
            }
        }
        IncomingState::IncomingOnly { incoming } => {
            // Only the staging record exists - this means it's the first time
            // we've ever seen it. No conflict possible, just take the remote.
            match incoming {
                Some(data) => IncomingAction::TakeRemote { data },
                None => IncomingAction::DeleteLocally,
            }
        }
    }
}

// Apply the actions necessary to fully process the incoming items.
pub fn apply_actions<S: ?Sized + Interruptee>(
    tx: &Transaction<'_>,
    actions: Vec<(IncomingItem, IncomingAction)>,
    signal: &S,
) -> Result<()> {
    for (item, action) in actions {
        signal.err_if_interrupted()?;

        log::trace!("action for '{}': {:?}", item.ext_id, action);
        // XXX - change counter should be updated consistently here!
        match action {
            IncomingAction::DeleteLocally => {
                // Can just nuke it entirely.
                tx.execute_named_cached(
                    "DELETE FROM storage_sync_data WHERE ext_id = :ext_id",
                    &[(":ext_id", &item.ext_id)],
                )?;
            }
            // We want to update the local record with 'data' and after this update the item no longer is considered dirty.
            IncomingAction::TakeRemote { data } => {
                tx.execute_named_cached(
                    "UPDATE storage_sync_data SET data = :data, sync_change_counter = 0 WHERE ext_id = :ext_id",
                    &[
                        (":ext_id", &item.ext_id),
                        (":data", &serde_json::Value::Object(data).as_str()),
                    ]
                )?;
            }

            // We merged this data, so need to update locally but still consider
            // it dirty because the merged data must be uploaded.
            IncomingAction::Merge { data } => {
                tx.execute_named_cached(
                    "UPDATE storage_sync_data SET data = :data, sync_change_counter = sync_change_counter + 1 WHERE ext_id = :ext_id",
                    &[
                        (":ext_id", &item.ext_id),
                        (":data", &serde_json::Value::Object(data).to_string()),
                    ]
                )?;
            }

            // Both local and remote ended up the same - only need to nuke the
            // change counter.
            IncomingAction::Same => {
                tx.execute_named_cached(
                    "UPDATE storage_sync_data SET sync_change_counter = 0 WHERE ext_id = :ext_id",
                    &[(":ext_id", &item.ext_id)],
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test::new_syncable_mem_db;
    use super::*;
    use crate::api;
    use interrupt_support::NeverInterrupts;
    use rusqlite::NO_PARAMS;
    use serde_json::{json, Value};

    // select simple int
    fn ssi(conn: &Connection, stmt: &str) -> u32 {
        conn.query_row_and_then(stmt, rusqlite::NO_PARAMS, |row| row.get::<_, u32>(0))
            .expect("must work")
    }

    fn array_to_incoming(mut array: Value) -> Vec<ServerPayload> {
        let jv = array.as_array_mut().expect("you must pass a json array");
        let mut result = Vec::with_capacity(jv.len());
        for elt in jv {
            result.push(serde_json::from_value(elt.take()).expect("must be valid"));
        }
        result
    }

    // Can't find a way to import this from crate::sync::tests...
    macro_rules! map {
        ($($map:tt)+) => {
            json!($($map)+).as_object().unwrap().clone()
        };
    }

    #[test]
    fn test_incoming_populates_staging() -> Result<()> {
        let db = new_syncable_mem_db();
        let mut conn = db.writer.lock().unwrap();
        let tx = conn.transaction()?;

        let incoming = json! {[
            {
                "guid": "guidAAAAAAAA",
                "last_modified": 0.0,
                "ext_id": "ext1@example.com",
                "data": json!({"foo": "bar"}).to_string(),
            }
        ]};

        stage_incoming(&tx, array_to_incoming(incoming), &NeverInterrupts)?;
        // check staging table
        assert_eq!(
            ssi(&tx, "SELECT count(*) FROM temp.storage_sync_staging"),
            1
        );
        Ok(())
    }

    #[test]
    fn test_fetch_incoming_state() -> Result<()> {
        let db = new_syncable_mem_db();
        let mut conn = db.writer.lock().unwrap();
        let tx = conn.transaction()?;

        // Start with an item just in staging.
        tx.execute(
            r#"
            INSERT INTO temp.storage_sync_staging (guid, ext_id, data)
            VALUES ('guid', 'ext_id', '{"foo":"bar"}')
        "#,
            NO_PARAMS,
        )?;

        let incoming = get_incoming(&tx)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].0,
            IncomingItem {
                guid: SyncGuid::new("guid"),
                ext_id: "ext_id".into()
            }
        );
        assert_eq!(
            incoming[0].1,
            IncomingState::IncomingOnly {
                incoming: Some(map!({"foo": "bar"})),
            }
        );

        // Add the same item to the mirror.
        tx.execute(
            r#"
            INSERT INTO storage_sync_mirror (guid, ext_id, data)
            VALUES ('guid', 'ext_id', '{"foo":"new"}')
        "#,
            NO_PARAMS,
        )?;
        let incoming = get_incoming(&tx)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::NotLocal {
                incoming: Some(map!({"foo": "bar"})),
                mirror: Some(map!({"foo": "new"})),
            }
        );

        // and finally the data itself - might as use the API here!
        api::set(&tx, "ext_id", json!({"foo": "local"}))?;
        let incoming = get_incoming(&tx)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::Everywhere {
                incoming: Some(map!({"foo": "bar"})),
                local: Some(map!({"foo": "local"})),
                mirror: Some(map!({"foo": "new"})),
            }
        );
        Ok(())
    }

    // Like test_fetch_incoming_state, but check NULLs are handled correctly.
    #[test]
    fn test_fetch_incoming_state_nulls() -> Result<()> {
        let db = new_syncable_mem_db();
        let mut conn = db.writer.lock().unwrap();
        let tx = conn.transaction()?;

        // Start with an item just in staging.
        tx.execute(
            r#"
            INSERT INTO temp.storage_sync_staging (guid, ext_id, data)
            VALUES ('guid', 'ext_id', NULL)
        "#,
            NO_PARAMS,
        )?;

        let incoming = get_incoming(&tx)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::IncomingOnly { incoming: None }
        );

        // Add the same item to the mirror.
        tx.execute(
            r#"
            INSERT INTO storage_sync_mirror (guid, ext_id, data)
            VALUES ('guid', 'ext_id', NULL)
        "#,
            NO_PARAMS,
        )?;
        let incoming = get_incoming(&tx)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::NotLocal {
                incoming: None,
                mirror: None,
            }
        );

        tx.execute(
            r#"
            INSERT INTO storage_sync_data (ext_id, data)
            VALUES ('ext_id', NULL)
        "#,
            NO_PARAMS,
        )?;
        let incoming = get_incoming(&tx)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::Everywhere {
                incoming: None,
                local: None,
                mirror: None,
            }
        );
        Ok(())
    }

    // XXX - test apply_actions!
}
