//! How far the crawl of a site has gone (`crawl.rs`).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "crawls")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    /// Where the next page of the walk is (a page number, an offset, a cursor of the site), as the crawl of that site writes it.
    pub cursor: String,
    /// The walk reached the end of the list: from now on only the newest pages are read.
    pub done: bool,
    /// How many works the crawl saved that were not saved before.
    pub saved: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
