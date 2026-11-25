//! A work id that was looked at because it is missing between the smallest and the largest id saved (the id check): it is not asked again.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "crawl_probes")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub checked_at: i64,
    /// The page exists and was saved (otherwise the page is gone).
    pub found: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
