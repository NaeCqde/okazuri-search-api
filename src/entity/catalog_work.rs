//! A work printed in an issue of a magazine (the Worker's `works` table). The copy has more columns; these are the ones read.
//! Named `catalog_work` because `work` is a work of the search database.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "works")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    /// The release date, in seconds since the epoch.
    pub date: i64,
    /// The place in the issue: 1, 2, 3, ...
    pub position: i64,
    pub number_id: String,
    pub author_id: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(belongs_to = "super::number::Entity", from = "Column::NumberId", to = "super::number::Column::Id")]
    Number,
    #[sea_orm(belongs_to = "super::catalog_author::Entity", from = "Column::AuthorId", to = "super::catalog_author::Column::Id")]
    Author,
}

impl Related<super::number::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Number.def()
    }
}

impl Related<super::catalog_author::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Author.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
