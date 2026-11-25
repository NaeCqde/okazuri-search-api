//! An account of a site that posts works (`work.uploader` is its `id` on the site): the uploader of E-Hentai, the circle (maker) of DLsite.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "uploaders")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    /// The id of the account on the site (the name in the address of E-Hentai, `RG01015077` of DLsite); the page of the account can be found from it.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The name the site shows.
    pub name: String,
    /// Its romaji: what a query is matched with.
    pub romaji: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
