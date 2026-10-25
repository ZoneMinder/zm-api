//! SeaORM migration runner CLI.
//!
//! The seed of the future `zm-api db` subcommand (issues #11/#12):
//!
//! ```text
//! # fresh install / normal migrations (sea-orm-migration CLI)
//! cargo run --bin migrator -- up -u mysql://root:pass@127.0.0.1:3307/somedb
//! cargo run --bin migrator -- status
//!
//! # bring an existing ZoneMinder database (>= 1.26.0) into the migration
//! # system: walk the embedded zm_update chain, converge triggers, stamp
//! # the baseline, run remaining migrations
//! cargo run --bin migrator -- bridge -u mysql://root:pass@host:3306/zm
//! ```

use sea_orm::{ConnectOptions, Database};

fn database_url(args: &[String]) -> String {
    for (i, a) in args.iter().enumerate() {
        if (a == "-u" || a == "--database-url") && i + 1 < args.len() {
            return args[i + 1].clone();
        }
    }
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return url;
    }
    zmconf_url().unwrap_or_else(|| {
        eprintln!("error: pass -u <url>, set DATABASE_URL, or run where zm.conf is readable");
        std::process::exit(2);
    })
}

/// The URL ZoneMinder's own `zm.conf` (and `conf.d`) describes, so the
/// account that runs ZoneMinder needs no `-u` (#147).
fn zmconf_url() -> Option<String> {
    let c = zm_api::configure::zmconf::ZmConfig::load();
    let (user, name) = (c.db_user()?, c.db_name()?);
    Some(zm_api::configure::db::DatabaseConfig::url_for_host(
        user,
        c.db_pass().unwrap_or_default(),
        c.db_host().unwrap_or("localhost"),
        c.db_port().unwrap_or(3306),
        name,
        c.db_socket(),
    ))
}

/// `status`: say first whether ZoneMinder's schema is behind this build.
async fn report_schema(url: &str) {
    let conn =
        match Database::connect(ConnectOptions::new(url).sqlx_logging(false).to_owned()).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: cannot connect: {e}");
                std::process::exit(1);
            }
        };
    match zm_api::migration::schema_check::check(&conn).await {
        Ok(state) => match state.explain() {
            Some(msg) => println!("DB behind binary: {msg}"),
            None => println!("DB schema is current for this binary"),
        },
        Err(e) => eprintln!("could not check the schema version: {e}"),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("bridge") {
        tracing_subscriber::fmt().with_target(false).init();
        let url = database_url(&args);
        // Single connection: the legacy chain uses session state
        // (SET @s ... PREPARE stmt FROM @s), which must not hop between
        // pooled connections.
        let mut opts = ConnectOptions::new(&url);
        opts.max_connections(1).sqlx_logging(false);
        let conn = match Database::connect(opts).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: cannot connect: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = zm_api::migration::legacy_bridge::run(&conn).await {
            eprintln!("bridge failed: {e}");
            std::process::exit(1);
        }
        println!("bridge complete: database is now on the migration system");
        return;
    }

    // The SeaORM CLI reads `-u` or DATABASE_URL; hand it zm.conf's when
    // neither is given.
    if !args.iter().any(|a| a == "-u" || a == "--database-url")
        && std::env::var("DATABASE_URL").is_err()
        && !args.iter().any(|a| a == "-h" || a == "--help")
    {
        if let Some(url) = zmconf_url() {
            std::env::set_var("DATABASE_URL", url);
        }
    }
    if args.first().map(String::as_str) == Some("status") {
        report_schema(&database_url(&args)).await;
    }
    sea_orm_migration::cli::run_cli(zm_api::migration::Migrator).await;
}
