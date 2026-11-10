//! When a query was asked to a site, and how it went.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "searches")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub query: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    pub searched_at: i64,
    /// False when the last try failed (the works of an earlier try are still saved).
    pub ok: bool,
    pub error: Option<String>,
    /// How many works the last good try listed.
    pub count: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
