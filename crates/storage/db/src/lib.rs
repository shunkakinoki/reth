#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxzy/reth/issues/"
)]
//! reth's database abstraction layer with concrete implementations.
//!
//! The database abstraction assumes that the underlying store is a KV store subdivided into tables.
//!
//! One or more changes are tied to a transaction that is atomically committed to the data store at
//! the same time. Strong consistency in what data is written and when is important for reth, so it
//! is not possible to write data to the database outside of using a transaction.
//!
//! Good starting points for this crate are:
//!
//! - [`Database`] for the main database abstraction
//! - [`DbTx`] (RO) and [`DbTxMut`] (RW) for the transaction abstractions.
//! - [`DbCursorRO`] (RO) and [`DbCursorRW`] (RW) for the cursor abstractions (see below).
//!
//! # Cursors and Walkers
//!
//! The abstraction also defines a couple of helpful abstractions for iterating and writing data:
//!
//! - **Cursors** ([`DbCursorRO`] / [`DbCursorRW`]) for iterating data in a table. Cursors are
//!   assumed to resolve data in a sorted manner when iterating from start to finish, and it is safe
//!   to assume that they are efficient at doing so.
//! - **Walkers** ([`Walker`] / [`RangeWalker`] / [`ReverseWalker`]) use cursors to walk the entries
//!   in a table, either fully from a specific point, or over a range.
//!
//! Dup tables (see below) also have corresponding cursors and walkers (e.g. [`DbDupCursorRO`]).
//! These **should** be preferred when working with dup tables, as they provide additional methods
//! that are optimized for dup tables.
//!
//! # Tables
//!
//! reth has two types of tables: simple KV stores (one key, one value) and dup tables (one key,
//! many values). Dup tables can be efficient for certain types of data.
//!
//! Keys are de/serialized using the [`Encode`] and [`Decode`] traits, and values are de/serialized
//! ("compressed") using the [`Compress`] and [`Decompress`] traits.
//!
//! Tables implement the [`Table`] trait.
//!
//! # Overview
//!
//! An overview of the current data model of reth can be found in the [`tables`] module.
//!
//! [`Database`]: crate::abstraction::database::Database
//! [`DbTx`]: crate::abstraction::transaction::DbTx
//! [`DbTxMut`]: crate::abstraction::transaction::DbTxMut
//! [`DbCursorRO`]: crate::abstraction::cursor::DbCursorRO
//! [`DbCursorRW`]: crate::abstraction::cursor::DbCursorRW
//! [`Walker`]: crate::abstraction::cursor::Walker
//! [`RangeWalker`]: crate::abstraction::cursor::RangeWalker
//! [`ReverseWalker`]: crate::abstraction::cursor::ReverseWalker
//! [`DbDupCursorRO`]: crate::abstraction::cursor::DbDupCursorRO
//! [`Encode`]: crate::abstraction::table::Encode
//! [`Decode`]: crate::abstraction::table::Decode
//! [`Compress`]: crate::abstraction::table::Compress
//! [`Decompress`]: crate::abstraction::table::Decompress
//! [`Table`]: crate::abstraction::table::Table
#![warn(missing_docs, unreachable_pub)]
#![deny(unused_must_use, rust_2018_idioms)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]
use eyre::WrapErr;
use reth_interfaces::db::LogLevel;
use std::path::Path;

/// Traits defining the database abstractions, such as cursors and transactions.
pub mod abstraction;
pub mod tables;
pub mod version;

mod implementation;
mod utils;

pub use abstraction::*;
pub use reth_interfaces::db::{DatabaseError, DatabaseWriteOperation};
pub use tables::*;
pub use utils::is_database_empty;

/// Bindings for [MDBX](https://libmdbx.dqdkfa.ru/).
#[cfg(feature = "mdbx")]
pub mod mdbx {
    pub use crate::implementation::mdbx::*;
    pub use reth_libmdbx::*;
}

#[cfg(feature = "mdbx")]
use mdbx::{Env, EnvKind, NoWriteMap, WriteMap};

/// Alias type for the database environment in use. Read/Write mode.
#[cfg(feature = "mdbx")]
pub type DatabaseEnv = Env<WriteMap>;

/// Alias type for the database engine in use. Read only mode.
#[cfg(feature = "mdbx")]
pub type DatabaseEnvRO = Env<NoWriteMap>;

/// Opens up an existing database or creates a new one at the specified path. Creates tables if
/// necessary. Read/Write mode.
pub fn init_db<P: AsRef<Path>>(path: P, log_level: Option<LogLevel>) -> eyre::Result<DatabaseEnv> {
    use crate::version::{check_db_version_file, create_db_version_file, DatabaseVersionError};

    let rpath = path.as_ref();
    if is_database_empty(rpath) {
        std::fs::create_dir_all(rpath)
            .wrap_err_with(|| format!("Could not create database directory {}", rpath.display()))?;
        create_db_version_file(rpath)?;
    } else {
        match check_db_version_file(rpath) {
            Ok(_) => (),
            Err(DatabaseVersionError::MissingFile) => create_db_version_file(rpath)?,
            Err(err) => return Err(err.into()),
        }
    }
    #[cfg(feature = "mdbx")]
    {
        let db = DatabaseEnv::open(rpath, EnvKind::RW, log_level)?;
        db.create_tables()?;
        return Ok(db)
    }

    unimplemented!();
}

/// Opens up an existing database. Read only mode. It doesn't create it or create tables if missing.
pub fn open_db_read_only(path: &Path, log_level: Option<LogLevel>) -> eyre::Result<DatabaseEnvRO> {
    #[cfg(feature = "mdbx")]
    {
        return Env::<NoWriteMap>::open(path, EnvKind::RO, log_level)
            .with_context(|| format!("Could not open database at path: {}", path.display()))
    }
    unimplemented!();
}

/// Opens up an existing database. Read/Write mode. It doesn't create it or create tables if
/// missing.
pub fn open_db(path: &Path, log_level: Option<LogLevel>) -> eyre::Result<DatabaseEnv> {
    #[cfg(feature = "mdbx")]
    {
        return Env::<WriteMap>::open(path, EnvKind::RW, log_level)
            .with_context(|| format!("Could not open database at path: {}", path.display()))
    }
    unimplemented!();
}

/// Collection of database test utilities
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils {
    use super::*;
    use std::sync::Arc;

    /// Error during database open
    pub const ERROR_DB_OPEN: &str = "Not able to open the database file.";

    /// Error during database creation
    pub const ERROR_DB_CREATION: &str = "Not able to create the database file.";

    /// Error during table creation
    pub const ERROR_TABLE_CREATION: &str = "Not able to create tables in the database.";

    /// Error during tempdir creation
    pub const ERROR_TEMPDIR: &str = "Not able to create a temporary directory.";

    /// Create read/write database for testing
    pub fn create_test_rw_db() -> Arc<DatabaseEnv> {
        Arc::new(
            init_db(tempfile::TempDir::new().expect(ERROR_TEMPDIR).into_path(), None)
                .expect(ERROR_DB_CREATION),
        )
    }

    /// Create read/write database for testing
    pub fn create_test_rw_db_with_path<P: AsRef<Path>>(path: P) -> Arc<DatabaseEnv> {
        Arc::new(init_db(path.as_ref(), None).expect(ERROR_DB_CREATION))
    }

    /// Create read only database for testing
    pub fn create_test_ro_db() -> Arc<DatabaseEnvRO> {
        let path = tempfile::TempDir::new().expect(ERROR_TEMPDIR).into_path();
        {
            init_db(path.as_path(), None).expect(ERROR_DB_CREATION);
        }
        Arc::new(open_db_read_only(path.as_path(), None).expect(ERROR_DB_OPEN))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        init_db,
        version::{db_version_file_path, DatabaseVersionError},
    };
    use assert_matches::assert_matches;
    use tempfile::tempdir;

    #[test]
    fn db_version() {
        let path = tempdir().unwrap();

        // Database is empty
        {
            let db = init_db(&path, None);
            assert_matches!(db, Ok(_));
        }

        // Database is not empty, current version is the same as in the file
        {
            let db = init_db(&path, None);
            assert_matches!(db, Ok(_));
        }

        // Database is not empty, version file is malformed
        {
            std::fs::write(path.path().join(db_version_file_path(&path)), "invalid-version")
                .unwrap();
            let db = init_db(&path, None);
            assert!(db.is_err());
            assert_matches!(
                db.unwrap_err().downcast_ref::<DatabaseVersionError>(),
                Some(DatabaseVersionError::MalformedFile)
            )
        }

        // Database is not empty, version file contains not matching version
        {
            std::fs::write(path.path().join(db_version_file_path(&path)), "0").unwrap();
            let db = init_db(&path, None);
            assert!(db.is_err());
            assert_matches!(
                db.unwrap_err().downcast_ref::<DatabaseVersionError>(),
                Some(DatabaseVersionError::VersionMismatch { version: 0 })
            )
        }
    }
}
