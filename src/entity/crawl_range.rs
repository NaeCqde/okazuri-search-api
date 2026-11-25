//! A range of pages of the list of a site that the first walk of the crawl has read (`crawls` keeps where the walk is; the pages are read in any order, by any worker).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "crawl_ranges")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    /// The first page of the range (a page of the list: its number, or for Hitomi.La the number of its batch).
    #[sea_orm(primary_key, auto_increment = false)]
    pub lo: i64,
    /// The last page of the range.
    pub hi: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
