//! What opening a team database does with the schema version it finds (PostgreSQL and MongoDB).
//!
//! The rules are the same for both, so they live here: a database that is not there is created
//! only by an import, one written by a newer server is left alone, and an older one is upgraded by
//! a writer. A read of an older database keeps working while the change was additive (see
//! `READABLE_FROM` of each backend's `schema`).

use crate::backend::Access;

/// What [`plan`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    /// No database: a read answers `None`.
    Absent,
    /// Used as it is.
    Current,
    /// Written by a newer server, left untouched.
    Newer,
    /// Too old for a read, which has to wait until a writer has upgraded it.
    TooOld,
    /// Migrated by this open: `creates` for a database that has no tables yet (quick), otherwise an
    /// upgrade of what is stored (slow on a real-sized database, so it gets an open of its own).
    Migrate { creates: bool },
}

/// What opening for `access` does with a database stored at `version` (`None`: it has none yet),
/// for a server that writes `current` and reads databases from `readable_from` on.
pub(crate) fn plan(version: Option<i64>, access: Access, current: i64, readable_from: i64) -> Plan {
    match (version, access) {
        (None, Access::Create) => Plan::Migrate { creates: true },
        (None, _) => Plan::Absent,
        (Some(version), _) if version > current => Plan::Newer,
        (Some(version), Access::Read) if version < readable_from => Plan::TooOld,
        (Some(_), Access::Read) => Plan::Current,
        (Some(version), _) if version < current => Plan::Migrate { creates: false },
        (Some(_), _) => Plan::Current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_import_creates_and_a_read_never_migrates() {
        assert_eq!(plan(None, Access::Create, 3, 2), Plan::Migrate { creates: true });
        assert_eq!(plan(None, Access::Existing, 3, 2), Plan::Absent);
        assert_eq!(plan(None, Access::Read, 3, 2), Plan::Absent);
        assert_eq!(plan(Some(2), Access::Read, 3, 2), Plan::Current);
        assert_eq!(plan(Some(3), Access::Read, 3, 2), Plan::Current);
    }

    #[test]
    fn an_older_database_stays_readable_down_to_the_oldest_version_a_read_copes_with() {
        assert_eq!(plan(Some(2), Access::Read, 3, 2), Plan::Current, "an additive change");
        assert_eq!(plan(Some(1), Access::Read, 3, 2), Plan::TooOld, "readers cannot cope");
    }

    #[test]
    fn a_writer_upgrades_an_older_database_and_leaves_a_newer_one_alone() {
        for access in [Access::Existing, Access::Create] {
            assert_eq!(plan(Some(1), access, 3, 2), Plan::Migrate { creates: false });
            assert_eq!(plan(Some(3), access, 3, 2), Plan::Current);
            assert_eq!(plan(Some(4), access, 3, 2), Plan::Newer);
        }

        assert_eq!(plan(Some(4), Access::Read, 3, 2), Plan::Newer);
    }
}
