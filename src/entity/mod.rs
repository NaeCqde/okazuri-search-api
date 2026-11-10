//! The tables, one entity (SeaORM) each.
//!
//! The search database (`[search] db_path`) is made by this program: `work`, `search` and `listing`.
//! The mirror (`[d1] mirror_path`) is a copy of tables that D1 owns, so `magazine` and `number` only list the columns that
//! are read (the copy has more); `mirror_meta` is ours, it says which data the copy was built from.

pub mod listing;
pub mod magazine;
pub mod mirror_meta;
pub mod number;
pub mod search;
pub mod work;
