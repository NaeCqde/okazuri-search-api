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
    pub first_seen: i64,
    pub updated_at: i64,
    /// The language the title says the work is in (`language.rs`): a two-letter code; NULL when the title says none.
    pub language: Option<String>,
    /// The page of the work is gone (`verify.rs`): the row stays as a mark, with no title, so that a list, the crawl and an import do not save
    /// the work again (the site still lists it).
    pub deleted: bool,
    /// When the page of the work was last looked at (`verify.rs`); NULL: never.
    pub checked_at: Option<i64>,
    /// The tags the list page shows, flat (no namespace; the category of E-Hentai is one), one per line; NULL when the site shows none.
    pub tags: Option<String>,
    /// The name of the uploader, when the list page shows it.
    pub uploader: Option<String>,
    /// When the work was posted (seconds since the epoch, UTC), when the list page shows it.
    pub posted_at: Option<i64>,
    /// The romaji of the uploader and the tags (the uploader on the first line): what a query is matched with, beside the title.
    pub extra_romaji: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
