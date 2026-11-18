//! An English title that Hitomi.La has for a title: one row for a gallery that answered a title (`english_lookups`).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "english_titles")]
pub struct Model {
    /// The title that was asked (`english_lookups.key`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    /// The id of the gallery on Hitomi.La.
    #[sea_orm(primary_key, auto_increment = false)]
    pub gallery_id: i64,
    /// The place in the answer (the titles that are the same first, then the newest).
    pub position: i64,
    /// The address of the gallery's page.
    pub url: String,
    /// The title the site gives: English, or the romaji.
    pub title: String,
    pub japanese_title: Option<String>,
    /// The text was the whole title.
    pub exact: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
