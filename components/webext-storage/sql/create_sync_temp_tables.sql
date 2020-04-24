-- This Source Code Form is subject to the terms of the Mozilla Public
-- License, v. 2.0. If a copy of the MPL was not distributed with this
-- file, You can obtain one at http://mozilla.org/MPL/2.0/.

-- Temp tables used by Sync.
-- Note that this is execute both before and after a sync.

CREATE TEMP TABLE IF NOT EXISTS storage_sync_staging (
    guid TEXT PRIMARY KEY,

    /* The extension_id is explicitly not the GUID used on the server.
       We may end up making this a regular foreign-key relationship back to
       storage_sync_data, although maybe not - the ext_id may not exist in
       storage_sync_data at the time we populate this table.
       We can iterate here as we site up sync support.
    */
    ext_id TEXT NOT NULL UNIQUE,

    /* Timestamp as recorded by the server - XXX - we don't currently use this
       for anything at all, so should consider killing it?
    */
    server_modified INTEGER NOT NULL,

    /* The JSON payload. We *do* allow NULL here - it means "deleted" */
    data TEXT
) WITHOUT ROWID;

DELETE FROM temp.storage_sync_staging;
