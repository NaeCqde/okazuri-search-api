//! Which query listed which work on which site: the work is gone from the database when no query lists it any more.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "listings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub query: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub path: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
