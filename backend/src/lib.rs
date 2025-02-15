mod routes;
mod log_reader;
mod middleware;

use std::collections::HashMap;
use ringbuffer::{AllocRingBuffer, RingBuffer};
use serde::Serialize;
use std::sync::Arc;
use time::OffsetDateTime;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use config::Config;
use lazy_static::lazy_static;
use log::{info, LevelFilter};
use logpeek::config::LoggingMode;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, System};
use tokio::sync::{Mutex, RwLock};
use routes::router_setup;

#[derive(Debug, Serialize, Clone)]
struct LogEntry {
    #[serde(with = "time::serde::rfc3339")]
    timestamp: OffsetDateTime,
    level: log::Level,
    module: String,
    message: String,
}

#[derive(Clone)]
struct SharedState {
    log_buffer: Arc<RwLock<AllocRingBuffer<LogEntry>>>,
    cache: Arc<Mutex<HashMap<String, (std::time::SystemTime, usize)>>>,
    last_buffer_update: Arc<Mutex<std::time::SystemTime>>,
    sys: Arc<Mutex<System>>,
    server_start_time: Arc<std::time::SystemTime>,
    os: Arc<String>,
    host_name: Arc<String>,
    login_attempts: Arc<Mutex<u32>>,
}

// Environment overrides the config file
lazy_static! {
    pub static ref SETTINGS: RwLock<Config> = RwLock::new(Config::builder()
        .add_source(config::File::with_name("config.toml").required(false))
        .add_source(config::Environment::with_prefix("LOGPEEK").separator("_"))
        .build()
        .expect("There is an issue with the configuration file"));
}

pub async fn run() {
    // Logger setup
    let logger_config = logpeek::config::Config {
        min_log_level: match SETTINGS.read().await.get_bool("main.logger.debug").unwrap_or(false) {
            true => LevelFilter::Debug,
            false => LevelFilter::Info
        },
        out_dir_name: logpeek::config::OutputDirName::Custom(SETTINGS.read().await.get_string("main.logger.log_path").unwrap_or_else(|_| "logpeek-logs".to_string())),
        logging_mode: match SETTINGS.read().await.get_bool("main.logger.log_to_file").unwrap_or(true) {
            true => LoggingMode::FileAndConsole,
            false => LoggingMode::Console
        },
        ..Default::default() };
    logpeek::init(logger_config).unwrap();

    info!("Starting...");

    //Read in and process the log entries
    let log_buffer = Arc::new(RwLock::new(AllocRingBuffer::new(SETTINGS.read().await.get_int("main.buffer_size").unwrap_or(1_000_000) as usize)));
    let cache = Arc::new(Mutex::new(HashMap::new()));

    log_reader::load_logs(log_buffer.clone(), cache.clone()).await;
    info!("Loaded {} log entries", log_buffer.read().await.len());

    // Initialize the system info
    let sys = Arc::new(Mutex::new(System::new_with_specifics(
        sysinfo::RefreshKind::new()
            .with_cpu(CpuRefreshKind::new().with_cpu_usage())
            .with_memory(MemoryRefreshKind::new().with_ram())
    )));
    let os = System::long_os_version().unwrap_or_default();
    let host_name = System::host_name().unwrap_or_default();

    let shared_state = SharedState {
        log_buffer,
        cache,
        last_buffer_update: Arc::new(Mutex::new(std::time::SystemTime::now())),
        sys,
        server_start_time: Arc::new(std::time::SystemTime::now()),
        os: Arc::new(os),
        host_name: Arc::new(host_name),
        login_attempts: Arc::new(Mutex::new(0)),
    };

    let host_address = SETTINGS.read().await.get_string("main.address").unwrap_or_else(|_| "127.0.0.1:3001".to_string());

    let app: Router = router_setup(shared_state).await;

    if SETTINGS.read().await.get_bool("https.enabled").unwrap_or(false) {
        let tls_config = RustlsConfig::from_pem_file(
            SETTINGS.read().await.get_string("https.cert").expect("Failed to read https.cert"),
            SETTINGS.read().await.get_string("https.key").expect("Failed to read https.key"),
        ).await.expect("Failed to create TLS config! Most likely there is an issue with the certificate or key file.");

        info!("Listening on https://{}", &host_address);

        axum_server::bind_rustls(host_address.parse().unwrap(), tls_config)
            .serve(app.into_make_service())
            .await
            .unwrap();
    } else {
        info!("Listening on http://{}", &host_address);
        axum_server::bind(host_address.parse().unwrap())
            .serve(app.into_make_service())
            .await
            .unwrap();
    }
}