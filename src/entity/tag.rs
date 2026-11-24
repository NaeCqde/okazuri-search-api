//! A tag of the works (`work.tags` lists them by `id`). The id is ours: the sites name a tag in their own ways, and the same tag of two sites is one row.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tags")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// The tag as the site writes it, flat and in lower case (`big breasts`). A name is unique with its kind (an artist and a tag may be named alike).
    pub name: String,
    /// What the tag is (`sites::TagKind`): 0 a tag, 1 an author, 2 a circle, 3 a weak tag.
    pub kind: i64,
    /// Its romaji: what a query is matched with.
    pub romaji: String,
    /// How much a match of this tag counts (1 is the normal score); it can be changed at any time.
    pub weight: f64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
