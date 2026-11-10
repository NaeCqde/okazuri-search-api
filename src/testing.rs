//! Shared by the tests. Loading the dictionary takes seconds, so it is done once for all of them.

use crate::api::AppState;
use crate::romanizer::Romanizer;
use std::sync::{Arc, OnceLock};

fn shared() -> &'static Arc<Romanizer> {
    static ROMANIZER: OnceLock<Arc<Romanizer>> = OnceLock::new();

    ROMANIZER.get_or_init(|| Arc::new(Romanizer::new(&Default::default()).expect("the embedded dictionary loads")))
}

pub fn romanizer() -> &'static Romanizer {
    shared()
}

/// The same romanizer, shared, for what takes one (the engine).
pub fn romanizer_arc() -> Arc<Romanizer> {
    shared().clone()
}

/// The state of the server, with the default limit of 300 characters.
pub fn romanizer_state() -> Arc<AppState> {
    Arc::new(AppState { romanizer: shared().clone(), max_query_chars: 300, search: None })
}
