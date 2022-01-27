use chrono::Utc;
use log::LevelFilter;
use std::env;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::Once;

/// Function for getting path to log file. For `cargo run` it will be path to project directory. In other cases it will be `./`
/// if binary file were called with path that contain `rusttradingengine` dir the log will be there
fn get_log_file_path(log_file: &str) -> PathBuf {
    let path_to_bin = std::env::args().next().expect("Failed to get first arg");

    PathBuf::from(path_to_bin)
        .ancestors()
        .find(|ancestor| ancestor.ends_with("rusttradingengine"))
        .unwrap_or(Path::new("./"))
        .join(log_file)
}

pub fn init_logger() {
    init_logger_file_named("log.txt")
}

pub fn init_logger_file_named(log_file: &str) {
    if let Ok(_) = env::var("MMB_NO_LOGS") {
        return;
    }

    let path = get_log_file_path(log_file);
    static INIT_LOGGER: Once = Once::new();

    INIT_LOGGER.call_once(|| {
        let _ = fern::Dispatch::new()
            .format(|out, message, record| {
                out.finish(format_args!(
                    "[{}][{}][{}] {}",
                    Utc::now().format("%Y-%m-%d %H:%M:%S,%3f"),
                    record.level(),
                    record.target(),
                    message
                ))
            })
            .chain(
                fern::Dispatch::new()
                    .level(LevelFilter::Warn)
                    .level_for("mmb", LevelFilter::Warn)
                    .level_for("mmb_core", LevelFilter::Warn)
                    .chain(std::io::stdout()),
            )
            .chain(
                fern::Dispatch::new()
                    .level(LevelFilter::Trace)
                    .level_for("actix_tls", LevelFilter::Warn)
                    .level_for("rustls", LevelFilter::Warn)
                    .level_for("actix_codec", LevelFilter::Warn)
                    .level_for("tungstenite", LevelFilter::Warn)
                    .level_for("tokio_tungstenite", LevelFilter::Warn)
                    .chain(
                        std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .open(path.clone())
                            .expect("Unable to open log file"),
                    ),
            )
            .apply()
            .expect("Unable to set up logger");
    });

    print_info(format!(
        "Logger has been initialized all logs will be stored here: {:?}",
        path
    ));
}

pub fn print_info<T>(msg: T)
where
    T: Display,
{
    log::info!("{msg}");
    println!("{msg}");
}
