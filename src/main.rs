/// A log line that has what a person searched for or what it was translated to: `[log] queries` says when the first form is written (never,
/// only a failure, always); otherwise the second one is (the same line without the text), so that the timings and the errors are still in the log.
macro_rules! text_log {
    ($level:ident, $with:literal $(, $a:expr)* ; $without:literal $(, $b:expr)* $(,)?) => {
        if crate::utils::log_text(stringify!($level)) {
            log::$level!($with $(, $a)*)
        } else {
            log::$level!($without $(, $b)*)
        }
    };
}

mod api;
mod cli;
mod config;
mod crawl;
mod cron;
mod d1;
mod dataset;
mod engine;
mod fill;
mod hitomi;
mod language;
mod entity;
mod gtranslate;
mod mirror;
mod pin;
mod prefetch;
mod query;
mod romaji;
mod romanizer;
mod sites;
mod store;
mod textindex;
#[cfg(test)]
mod testing;
mod titles;
mod tokenizer;
mod transfer;
mod utils;
mod verify;

use std::env;
use std::process::exit;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let result = match cli::parse(&args) {
        Ok(command) => cli::run(command).await,
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        eprintln!("error: {}", e);
        exit(1);
    }
}
