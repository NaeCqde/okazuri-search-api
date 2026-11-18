//! A translation that Hitomi.La has for a text: one row for a gallery that answered it (`translation_lookups`).
//! The gallery has its title in English and in Japanese, so the pair is the translation, in both directions.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "translations")]
pub struct Model {
    /// The text that was asked (`translation_lookups.key`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    /// The id of the gallery on Hitomi.La.
    #[sea_orm(primary_key, auto_increment = false)]
    pub gallery_id: i64,
    /// The place in the answer (the titles that are the same first, then the newest).
    pub position: i64,
    /// The address of the gallery's page.
    pub url: String,
    pub english: String,
    pub japanese: String,
    /// `english` and `japanese` as they are compared (`hitomi::squash`), to find a translation from the other side.
    pub english_key: String,
    pub japanese_key: String,
    /// The text was the whole title.
    pub exact: bool,
    /// Where it comes from: `hitomi` (the titles of a work) or `google` (Google Translate: no gallery, `gallery_id` 0).
    pub source: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
