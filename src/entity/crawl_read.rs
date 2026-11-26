//! When a page of the list of a site was read by the first walk of the crawl (the gap check, `crawl.rs`). Made with `CREATE TABLE IF NOT EXISTS` in `store.rs`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "crawl_reads")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub page: i64,
    /// A number that is different for every job that read pages (the pages of one job are read in order).
    pub job: i64,
    /// The earliest time (unix ms, the clock of the host) the page may have been read.
    pub lo_ms: i64,
    /// The latest time the page may have been read.
    pub hi_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
