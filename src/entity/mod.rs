//! The tables, one entity (SeaORM) each.
//!
//! The search database (`[search] db_path`) is made by this program: `work`, `search`, `listing`, `translation_lookup` and `translation`.
//! The mirror (`[magazine] mirror_path`) is a copy of tables that D1 owns, so `magazine`, `number`, `catalog_work` and `catalog_author` only list the columns that
//! are read (the copy has more); `mirror_meta` is ours, it says which data the copy was built from.

pub mod catalog_author;
pub mod catalog_work;
pub mod crawl_state;
pub mod listing;
pub mod magazine;
pub mod mirror_meta;
pub mod number;
pub mod search;
pub mod site_tag_name;
pub mod tag;
pub mod translation;
pub mod translation_lookup;
pub mod uploader;
pub mod work;
