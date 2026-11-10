//! An issue of a magazine. The copy has more columns; these are the ones that are read.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "numbers")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The name every issue of a magazine shares (`COMIC快楽天 2026年07月号`).
    pub name: String,
    /// The release date, in seconds since the epoch.
    pub date: i64,
    pub magazine_id: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::magazine::Entity",
        from = "Column::MagazineId",
        to = "super::magazine::Column::Id"
    )]
    Magazine,
}

impl Related<super::magazine::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Magazine.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
