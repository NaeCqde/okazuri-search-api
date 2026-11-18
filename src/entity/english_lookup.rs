//! That a title was asked to Hitomi.La for its English titles, and when (so that a title with no answer is not asked every time).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "english_lookups")]
pub struct Model {
    /// The text as it is compared: letters and digits in lower case (`hitomi::squash`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    /// The text as it was asked.
    pub text: String,
    pub looked_at: i64,
    /// How many galleries answered.
    pub count: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
