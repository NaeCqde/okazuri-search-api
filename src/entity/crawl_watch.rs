//! The last look at the top of the list of a site while its first walk is not done (the gap check): page 1 as the host read it.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "crawl_watch")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    /// When the watch began (unix ms): the top is known from then on.
    pub since0: i64,
    /// When the top was looked at last.
    pub at: i64,
    /// The paths of the works of page 1 joined by a newline (Hitomi.La: the length of the index file in bytes, as a number).
    pub top: String,
    /// How many works a page shows.
    pub per_page: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
