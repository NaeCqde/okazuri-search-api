//! The name of a number a site gives a tag (`data-id` of AsmHentai, the id of a WordPress tag), learned by the crawl.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "site_tag_names")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub website: String,
    /// What the number is of on the site (`tags`, `artists`, ...).
    #[sea_orm(primary_key, auto_increment = false)]
    pub attribute: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
