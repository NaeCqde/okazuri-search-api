//! The pages to read again after the first walk of a site (the gap check): one row for each span `lo..=hi`, made once.
//! The row `(0, 0)` with `done_at` set says that the plan was made and there is nothing to read.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "crawl_gap_plan")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub lo: i64,
    pub hi: i64,
    /// When the span was read again (or given up); none while it waits.
    pub done_at: Option<i64>,
    /// How many times reading it failed.
    pub tries: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
