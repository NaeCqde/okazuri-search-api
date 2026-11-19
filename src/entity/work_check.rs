//! Whether the page of a work was looked at and is still there (`engine.rs`, `verified`): a work whose page is gone is not saved again,
//! whatever lists it (the site still lists it, and the crawl, an import and a search would put it back).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "work_checks")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub path: String,
    pub checked_at: i64,
    /// The page of the work is gone: the work is not in `works` and is not saved again.
    pub gone: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
