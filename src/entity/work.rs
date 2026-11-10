//! A work the list page of a site shows: one row for a page, whatever query found it.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "works")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    /// The path of the work's page, without the scheme and the domain.
    #[sea_orm(primary_key, auto_increment = false)]
    pub path: String,
    /// The title as the site writes it.
    pub title: String,
    /// Its romaji: what a query (which is always romaji) is matched with.
    pub title_romaji: String,
    /// The romaji as it is compared (case, width and white space folded), to narrow a search in SQL.
    pub title_key: String,
    pub first_seen: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
