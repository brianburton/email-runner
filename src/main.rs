use flexi_logger::{
    Age, Cleanup, Criterion, FileSpec, FlexiLoggerError, Logger, Naming, opt_format,
};
use im::Vector;
use log::{error, info};
use serde::Deserialize;
use std::cmp;
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::ops::Add;
use std::process::{Command, ExitStatus};
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{env, io};
use thiserror::Error;

#[derive(Debug, Error)]
enum AppError {
    #[error("usage: email_launcher config-file")]
    Usage,
    #[error("Error reading config file {0}: {1}")]
    ConfigFileError(String, #[source] Box<dyn Error>),
    #[error("Error configuring logging: {0}")]
    LoggingSetupFailed(FlexiLoggerError),
    #[error("Error running email sync program {0}: {1}")]
    SyncSpawnFailed(String, #[source] io::Error),
    #[error("Bad status from email sync program {0}: {1}")]
    SyncBadExitStatus(String, ExitStatus),
    #[error("Error running email client program {0}: {1}")]
    ClientSpawnFailed(String, #[source] io::Error),
    #[error("Error waiting for email client program {0}: {1}")]
    ClientWaitFailed(String, #[source] io::Error),
    #[error("Bad status from email client program {0}: {1}")]
    ClientBadExitStatus(String, ExitStatus),
    #[error("Error running email client program {0}: {1}")]
    SyncChannelError(String, #[source] mpsc::SendError<bool>),
    #[error("Error running joining client program {0}")]
    SyncJoinError(String),
}

#[derive(Clone, Deserialize)]
struct MailboxConfig {
    program: String,
    args: Vector<String>,
    interval_minutes: u64,
}

#[derive(Deserialize)]
struct EmailClientConfig {
    program: String,
    args: Vector<String>,
}

#[derive(Deserialize)]
struct AppConfig {
    client: EmailClientConfig,
    mailboxes: Vector<MailboxConfig>,
    log_dir: String,
}

impl MailboxConfig {
    fn summary(&self) -> String {
        format!("{} {:?}", self.program, self.args)
    }
}

impl EmailClientConfig {
    fn summary(&self) -> String {
        format!("{} {:?}", self.program, self.args)
    }
}

fn run_sync(config: &MailboxConfig) -> Result<(), AppError> {
    let rc = Command::new(&config.program)
        .args(&config.args)
        .output()
        .map_err(|e| AppError::SyncSpawnFailed(config.summary(), e))?;
    let stdout = String::from_utf8_lossy(&rc.stdout);
    let stderr = String::from_utf8_lossy(&rc.stderr);
    info!("Sync {} complete: status: {}", config.summary(), rc.status);
    if !stdout.is_empty() {
        info!(
            "Sync {} complete: stdout:\n{}",
            config.summary(),
            stdout.trim_end()
        );
    }
    if !stderr.is_empty() {
        info!(
            "Sync {} complete: stderr:\n{}",
            config.summary(),
            stderr.trim_end()
        );
    }
    if !rc.status.success() {
        return Err(AppError::SyncBadExitStatus(config.summary(), rc.status));
    }
    Ok(())
}

struct SyncThreadControl {
    handle: JoinHandle<()>,
    channel: mpsc::Sender<bool>,
}

fn spawn_sync_thread(config: &MailboxConfig) -> Result<SyncThreadControl, AppError> {
    let config = config.clone();
    let (send, recv) = mpsc::channel();
    let handle = thread::spawn(move || {
        info!("Sync thread {} started", config.summary());
        let interval_seconds = cmp::max(1, config.interval_minutes) * 60;
        let timeout_duration = Duration::from_millis(100);
        let interval_duration = Duration::from_secs(interval_seconds);
        let mut next_instant = Instant::now();
        loop {
            let current_instant = Instant::now();
            let sync_needed = current_instant >= next_instant
                || match recv.recv_timeout(timeout_duration) {
                    Ok(true) => true,
                    Err(mpsc::RecvTimeoutError::Timeout) => false,
                    Ok(false) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
            if sync_needed {
                info!("Sync program {} started", config.summary());
                let rc = run_sync(&config);
                info!("Sync program {} stopped", config.summary());
                if let Err(err) = rc {
                    error!("Sync program {} returned error: {}", config.summary(), err);
                }
                next_instant = current_instant.add(interval_duration);
            }
        }
        info!("Sync thread {} stopped", config.summary());
    });
    Ok(SyncThreadControl {
        handle,
        channel: send,
    })
}

fn run_email_client(config: &EmailClientConfig) -> Result<(), AppError> {
    info!("Email client {} started", config.summary());
    let status = Command::new(&config.program)
        .args(&config.args)
        .spawn()
        .map_err(|e| AppError::ClientSpawnFailed(config.summary(), e))?
        .wait()
        .map_err(|e| AppError::ClientWaitFailed(config.summary(), e))?;
    if !status.success() {
        return Err(AppError::ClientBadExitStatus(
            config.summary().to_string(),
            status,
        ));
    }
    info!("Email client {} stopped", config.summary());
    Ok(())
}

fn read_config(path: &str) -> Result<AppConfig, Box<dyn Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let config: AppConfig = serde_yml::from_reader(reader)?;
    Ok(config)
}

fn main() -> Result<(), AppError> {
    let config_file = env::args().nth(1).ok_or(AppError::Usage)?;
    let config =
        read_config(&config_file).map_err(|e| AppError::ConfigFileError(config_file, e))?;
    Logger::try_with_str("info")
        .unwrap()
        .log_to_file(FileSpec::default().directory(config.log_dir))
        .rotate(
            Criterion::Age(Age::Day),
            Naming::Timestamps,
            Cleanup::KeepLogFiles(7),
        )
        .format(opt_format)
        .start()
        .map_err(AppError::LoggingSetupFailed)?;
    let sync_threads = config
        .mailboxes
        .iter()
        .map(spawn_sync_thread)
        .collect::<Vec<Result<SyncThreadControl, AppError>>>()
        .into_iter()
        .collect::<Result<Vec<_>, AppError>>()?;
    let client_result = run_email_client(&config.client);
    sync_threads
        .iter()
        .map(|sc| {
            sc.channel
                .send(false)
                .map_err(|e| AppError::SyncChannelError(config.client.summary(), e))
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    sync_threads
        .into_iter()
        .map(|sc| {
            sc.handle
                .join()
                .map_err(|_| AppError::SyncJoinError(config.client.summary()))
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    client_result
}
