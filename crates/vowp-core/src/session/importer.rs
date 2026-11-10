//! Claude Code transcript importer (`vowp session watch`), optional in v0.1.
//!
//! The CLI ships a limited importer: `vowp session watch --once` scans a Claude
//! transcript directory, appends the new lines to a canonical session, and records
//! the processed line count in the node database. Continuous `--follow` is not
//! implemented in this build, and this module is left as an explicit, documented
//! open item rather than a stub that pretends to work.
