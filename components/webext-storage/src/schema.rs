/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// XXXXXX - This has been cloned from places/src/schema.rs, which has the
// comment:
// // This has been cloned from logins/src/schema.rs, on Thom's
// // wip-sync-sql-store branch.
// // We should work out how to turn this into something that can use a shared
// // db.rs.
//
// And we really should :) But not now.

use crate::error::Result;
use rusqlite::{Connection, NO_PARAMS};
use sql_support::ConnExt;

const VERSION: i64 = 1; // let's avoid bumping this and migrating for now!

const CREATE_SCHEMA_SQL: &str = include_str!("../sql/create_schema.sql");
const CREATE_TEMP_TABLES_SQL: &str = include_str!("../sql/create_temp_tables.sql");

fn get_current_schema_version(db: &Connection) -> Result<i64> {
    Ok(db.query_one::<i64>("PRAGMA user_version")?)
}

pub fn init(db: &Connection) -> Result<()> {
    let user_version = get_current_schema_version(db)?;
    if user_version == 0 {
        create(db)?;
    } else if user_version != VERSION {
        if user_version < VERSION {
            panic!("no migrations yet!");
        } else {
            log::warn!(
                "Loaded future schema version {} (we only understand version {}). \
                 Optimistically ",
                user_version,
                VERSION
            );
            // Downgrade the schema version, so that anything added with our
            // schema is migrated forward when the newer library reads our
            // database.
            db.execute_batch(&format!("PRAGMA user_version = {};", VERSION))?;
        }
        create(db)?;
    }
    create_temp_tables(db)?;
    Ok(())
}

fn create(db: &Connection) -> Result<()> {
    log::debug!("Creating schema");
    db.execute_batch(CREATE_SCHEMA_SQL)?;
    db.execute(
        &format!("PRAGMA user_version = {version}", version = VERSION),
        NO_PARAMS,
    )?;

    Ok(())
}

fn create_temp_tables(db: &Connection) -> Result<()> {
    log::debug!("Creating temp tables");
    db.execute_batch(CREATE_TEMP_TABLES_SQL)?;
    Ok(())
}

#[cfg(test)]
pub mod test {
    use prettytable::{Cell, Row};
    use rusqlite::Result as RusqliteResult;
    use rusqlite::{types::Value, Connection, NO_PARAMS};

    // To help debugging tests etc.
    #[allow(unused)]
    pub fn print_table(conn: &Connection, table_name: &str) -> RusqliteResult<()> {
        let mut stmt = conn.prepare(&format!("SELECT * FROM {}", table_name))?;
        let mut rows = stmt.query(NO_PARAMS)?;
        let mut table = prettytable::Table::new();
        let mut titles = Row::empty();
        for col in rows.columns().expect("must have columns") {
            titles.add_cell(Cell::new(col.name()));
        }
        table.set_titles(titles);
        while let Some(sql_row) = rows.next()? {
            let mut table_row = Row::empty();
            for i in 0..sql_row.column_count() {
                let val = match sql_row.get::<_, Value>(i)? {
                    Value::Null => "null".to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => f.to_string(),
                    Value::Text(s) => s,
                    Value::Blob(b) => format!("<blob with {} bytes>", b.len()),
                };
                table_row.add_cell(Cell::new(&val));
            }
            table.add_row(table_row);
        }
        table.printstd();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test::new_mem_db;

    #[test]
    fn test_create_schema_twice() {
        let db = new_mem_db();
        let conn = db.writer.lock().unwrap();
        conn.execute_batch(CREATE_SCHEMA_SQL)
            .expect("should allow running twice");
    }
}
