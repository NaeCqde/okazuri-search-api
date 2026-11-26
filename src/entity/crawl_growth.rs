//! A look at the top of the list that saw works added (the gap check): between `since` and `at` (unix ms) `added` works came to the top.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "crawl_growth")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    pub since: i64,
    #[sea_orm(primary_key, auto_increment = false)]
    pub at: i64,
    pub added: i64,
    /// No work of the page before was on page 1 any more: more works came than the page shows, and `added` is only a lower bound.
    pub saturated: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
