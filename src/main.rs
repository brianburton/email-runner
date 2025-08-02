use flexi_logger::{
    Age, Cleanup, Criterion, FileSpec, FlexiLoggerError, Logger, Naming, opt_format,
};
use im::Vector;
use log::{error, info};
use serde::Deserialize;
use std::error::Error;
use std::fs::File;
use std::io::{BufReader, Read};
use std::ops::Add;
use std::process::{Command, ExitStatus};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{cmp, fs};
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
    SyncChannelError(String, #[source] mpsc::SendError<SyncCommand>),
    #[error("Error running joining client program {0}")]
    SyncJoinError(String),
}

#[derive(Copy, Clone)]
enum SyncCommand {
    Update,
    Idle,
    Stop,
}

#[derive(Clone)]
struct SyncThreadSender {
    thread_name: String,
    channel: mpsc::Sender<SyncCommand>,
}

struct SyncThreadControl {
    handle: JoinHandle<()>,
    sender: SyncThreadSender,
}

#[derive(Clone, Deserialize, Debug)]
struct MailboxConfig {
    program: String,
    args: Vector<String>,
    interval_minutes: u64,
}

#[derive(Deserialize, Debug)]
struct EmailClientConfig {
    program: String,
    args: Vector<String>,
}

#[derive(Deserialize, Debug)]
struct AppConfig {
    client: EmailClientConfig,
    mailboxes: Vector<MailboxConfig>,
    log_dir: String,
    force_sync_path: String,
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

fn read_sync_command(
    channel: &Receiver<SyncCommand>,
    timeout_duration: Duration,
    sync_needed: bool,
) -> SyncCommand {
    if sync_needed {
        SyncCommand::Update
    } else {
        match channel.recv_timeout(timeout_duration) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => SyncCommand::Idle,
            Err(mpsc::RecvTimeoutError::Disconnected) => SyncCommand::Stop,
        }
    }
}

fn send_to_sync_channels(
    sync_thread_channels: &[SyncThreadSender],
    value_to_send: SyncCommand,
) -> Result<(), AppError> {
    sync_thread_channels
        .iter()
        .map(|sc| {
            sc.channel
                .send(value_to_send)
                .map_err(|e| AppError::SyncChannelError(sc.thread_name.to_owned(), e))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|_| ())
}

fn spawn_generic_sync_thread<F1, F2>(
    thread_name: &str,
    mut pretest: F1,
    mut update: F2,
) -> Result<SyncThreadControl, AppError>
where
    F1: FnMut() -> bool + Send + 'static,
    F2: FnMut() + Send + 'static,
{
    let moved_thread_name = thread_name.to_owned();
    let (send, recv) = mpsc::channel();
    let handle = thread::spawn(move || {
        info!("Sync thread {moved_thread_name} started");
        let timeout_duration = Duration::from_millis(100);
        loop {
            match read_sync_command(&recv, timeout_duration, pretest()) {
                SyncCommand::Idle => (),
                SyncCommand::Stop => break,
                SyncCommand::Update => update(),
            }
        }
        info!("Sync thread {moved_thread_name} stopped");
    });
    Ok(SyncThreadControl {
        handle,
        sender: SyncThreadSender {
            thread_name: thread_name.to_owned(),
            channel: send,
        },
    })
}

fn spawn_sync_thread(config: &MailboxConfig) -> Result<SyncThreadControl, AppError> {
    let interval_seconds = cmp::max(1, config.interval_minutes) * 60;
    let interval_duration = Duration::from_secs(interval_seconds);
    let next_instant = Arc::new(Mutex::new(Instant::now()));
    let pretest = {
        let next_instant = next_instant.clone();
        move || Instant::now() >= *next_instant.lock().unwrap()
    };

    let thread_name = config.summary().to_owned();
    let config = config.clone();
    let update = {
        let next_instant = next_instant.clone();
        move || {
            info!("Sync program {} started", config.summary());
            let rc = run_sync(&config);
            info!("Sync program {} stopped", config.summary());
            if let Err(err) = rc {
                error!("Sync program {} returned error: {}", config.summary(), err);
            }
            let mut next_instant = next_instant.lock().unwrap();
            *next_instant = Instant::now().add(interval_duration);
        }
    };

    spawn_generic_sync_thread(&thread_name, pretest, update)
}

fn spawn_force_sync_thread(
    sync_file_path: String,
    senders: Vec<SyncThreadSender>,
) -> Result<SyncThreadControl, AppError> {
    let pretest = {
        let sync_file_path = sync_file_path.clone();
        move || fs::exists(&sync_file_path).ok().unwrap_or(false)
    };

    let update = move || {
        info!("Force sync requested");
        let rc = send_to_sync_channels(&senders, SyncCommand::Update);
        if let Err(err) = rc {
            error!("Force sync returned error: {err}");
        }
        if let Err(err) = fs::remove_file(&sync_file_path) {
            error!("Force sync unable to remove {sync_file_path}: {err}");
        }
    };

    spawn_generic_sync_thread("force-sync-thread", pretest, update)
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
    let mut raw_config_string = String::new();
    let config_file = File::open(path)?;
    BufReader::new(config_file).read_to_string(&mut raw_config_string)?;
    raw_config_string = raw_config_string.replace("~", "${HOME}");
    let expanded_config_string = shellexpand::full(&&raw_config_string).map(|v| v.to_string())?;
    let config = serde_yml::from_str(&expanded_config_string)?;
    Ok(config)
}

fn join_threads(config: AppConfig, sync_threads: Vec<SyncThreadControl>) -> Result<(), AppError> {
    sync_threads
        .into_iter()
        .map(|sc| {
            sc.handle
                .join()
                .map_err(|_| AppError::SyncJoinError(config.client.summary()))
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(())
}

fn stop_threads(sync_threads: &[SyncThreadControl]) -> Result<(), AppError> {
    info!("Stopping all threads");
    let senders = sync_threads
        .iter()
        .map(|c| c.sender.clone())
        .collect::<Vec<_>>();
    send_to_sync_channels(&senders, SyncCommand::Stop)
}

fn start_force_sync_thread(
    config: &AppConfig,
    sync_threads: &[SyncThreadControl],
) -> Result<SyncThreadControl, AppError> {
    let senders = sync_threads
        .iter()
        .map(|sc| sc.sender.clone())
        .collect::<Vec<_>>();
    let force_sync_thread = spawn_force_sync_thread(config.force_sync_path.to_string(), senders)?;
    Ok(force_sync_thread)
}

fn start_sync_threads(config: &AppConfig) -> Result<Vec<SyncThreadControl>, AppError> {
    config
        .mailboxes
        .iter()
        .map(spawn_sync_thread)
        .collect::<Vec<Result<SyncThreadControl, AppError>>>()
        .into_iter()
        .collect::<Result<Vec<_>, AppError>>()
}

fn set_up_logging(config: &AppConfig) -> Result<(), AppError> {
    if !std::path::Path::new(&config.log_dir).exists() {
        return Err(AppError::Usage);
    }
    let log_dir_spec = FileSpec::default().directory(config.log_dir.as_str());
    Logger::try_with_str("info")
        .unwrap()
        .log_to_file(log_dir_spec)
        .rotate(
            Criterion::Age(Age::Day),
            Naming::Timestamps,
            Cleanup::KeepLogFiles(7),
        )
        .format(opt_format)
        .start()
        .map_err(AppError::LoggingSetupFailed)?;
    Ok(())
}

fn main() -> Result<(), AppError> {
    let config_file = env::args().nth(1).ok_or(AppError::Usage)?;
    let config =
        read_config(&config_file).map_err(|e| AppError::ConfigFileError(config_file, e))?;
    set_up_logging(&config)?;

    let mut sync_threads = start_sync_threads(&config)?;
    let force_sync_thread = start_force_sync_thread(&config, &sync_threads)?;
    sync_threads.push(force_sync_thread);

    let client_result = run_email_client(&config.client);

    stop_threads(&sync_threads)?;
    join_threads(config, sync_threads)?;
    client_result
}
