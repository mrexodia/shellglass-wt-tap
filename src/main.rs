#[cfg(all(feature = "push", windows))]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(feature = "push", not(windows)))]
use anyhow::bail;
use clap::{Parser, Subcommand};
#[cfg(any(feature = "serve", feature = "push"))]
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "shellglass-wt-tap",
    version,
    about = "Private-ABI Windows Terminal source for shellglass"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[cfg(feature = "serve")]
    Serve {
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        bind: String,
        #[arg(short, long, env = "SHELLGLASS_CONFIG")]
        config: Option<PathBuf>,
        /// Stop capture when focus leaves every known terminal window.
        #[arg(long)]
        foreground_only: bool,
        #[arg(long = "cors-origin")]
        cors_origin: Vec<String>,
        #[arg(long)]
        ssh_bind: Option<String>,
        #[arg(long)]
        ssh_host_key: Option<PathBuf>,
        #[arg(long)]
        ssh_motd_file: Option<PathBuf>,
        #[arg(long, default_value_t = 5)]
        ssh_motd_delay: u64,
        #[arg(long)]
        record_dir: Option<PathBuf>,
    },
    #[cfg(feature = "push")]
    Push {
        url: String,
        #[arg(
            long,
            env = "SHELLGLASS_KEY",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        key: String,
        #[arg(short, long, env = "SHELLGLASS_CONFIG")]
        config: Option<PathBuf>,
        /// Stop capture when focus leaves every known terminal window.
        #[arg(long)]
        foreground_only: bool,
        #[arg(long)]
        no_record: bool,
    },
    #[cfg(feature = "push")]
    Stream {
        #[command(subcommand)]
        command: StreamCommand,
    },
}

#[cfg(feature = "push")]
#[derive(Subcommand)]
enum StreamCommand {
    Start {
        #[arg(long)]
        hub: String,
        #[arg(
            long,
            env = "SHELLGLASS_KEY",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        key: String,
        #[arg(short, long, env = "SHELLGLASS_CONFIG")]
        config: Option<PathBuf>,
        /// Stop capture when focus leaves every known terminal window.
        #[arg(long)]
        foreground_only: bool,
        #[arg(long)]
        no_record: bool,
    },
    Pause,
    Resume,
    Stop,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        #[cfg(feature = "serve")]
        Command::Serve {
            bind,
            config,
            foreground_only,
            cors_origin,
            ssh_bind,
            ssh_host_key,
            ssh_motd_file,
            ssh_motd_delay,
            record_dir,
        } => {
            let presentation = shellglass::api::Presentation::load(config.as_deref())?;
            let mut options = shellglass::api::ServeOptions::new(bind);
            options.cors_origins = cors_origin;
            options.ssh_bind = ssh_bind;
            options.ssh_host_key = ssh_host_key;
            options.ssh_motd_file = ssh_motd_file;
            options.ssh_motd_delay = ssh_motd_delay;
            options.record_dir = record_dir;
            options.source_label = "the active Windows terminal".into();
            shellglass::api::serve(
                move || start_source(!foreground_only),
                presentation,
                options,
            )
            .await
        }
        #[cfg(feature = "push")]
        Command::Push {
            url,
            key,
            config,
            foreground_only,
            no_record,
        } => {
            #[cfg(windows)]
            if std::env::var_os("SHELLGLASS_WT_STREAM_WORKER").is_some() {
                shellglass_wt_tap::windows_native::start_control_server()?;
            }
            println!(
                "shellglass-wt-tap: pushing live to {}",
                url.trim_end_matches('/')
            );
            let presentation = shellglass::api::Presentation::load(config.as_deref())?;
            let mut options = shellglass::api::PushOptions::new(url, key);
            options.no_record = no_record;
            shellglass::api::push(
                move || start_source(!foreground_only),
                presentation,
                options,
            )
            .await
        }
        #[cfg(feature = "push")]
        Command::Stream { command } => run_stream(command).await,
    }
}

#[cfg(any(feature = "serve", feature = "push"))]
fn start_source(keep_last_terminal: bool) -> Result<shellglass::source::SourceSession> {
    #[cfg(windows)]
    {
        shellglass_wt_tap::windows_native::start(keep_last_terminal)
    }
    #[cfg(not(windows))]
    {
        let _ = keep_last_terminal;
        bail!("Windows Terminal capture is available only on Windows")
    }
}

#[cfg(all(feature = "push", windows))]
async fn run_stream(command: StreamCommand) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command as ProcessCommand, Stdio};

    match command {
        StreamCommand::Start {
            hub,
            key,
            config,
            foreground_only,
            no_record,
        } => {
            if shellglass_wt_tap::windows_native::control("status")
                .await
                .is_ok()
            {
                bail!("a detached shellglass WT stream is already running");
            }
            let exe = std::env::current_exe().context("locating shellglass-wt-tap")?;
            let mut child = ProcessCommand::new(exe);
            child
                .arg("push")
                .arg(hub)
                // Keep the long-lived secret out of Win32 process command-line
                // inspection. Clap reads the same key from this hidden env input.
                .env("SHELLGLASS_KEY", key)
                .env("SHELLGLASS_WT_STREAM_WORKER", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
            if let Some(config) = config {
                child.arg("--config").arg(config);
            }
            if foreground_only {
                child.arg("--foreground-only");
            }
            if no_record {
                child.arg("--no-record");
            }
            let mut child = child
                .spawn()
                .context("launching detached WT stream worker")?;
            for _ in 0..100 {
                if let Ok(status) = shellglass_wt_tap::windows_native::control("status").await {
                    print!("shellglass-wt-tap stream: {status}");
                    return Ok(());
                }
                if let Some(status) = child.try_wait()? {
                    bail!("detached WT stream worker exited during startup ({status})");
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            bail!("detached WT stream worker did not become ready")
        }
        StreamCommand::Pause => stream_control("pause").await,
        StreamCommand::Resume => stream_control("resume").await,
        StreamCommand::Stop => stream_control("stop").await,
        StreamCommand::Status => stream_control("status").await,
    }
}

#[cfg(all(feature = "push", windows))]
async fn stream_control(command: &str) -> Result<()> {
    let response = shellglass_wt_tap::windows_native::control(command).await?;
    print!("shellglass-wt-tap stream: {response}");
    Ok(())
}

#[cfg(all(feature = "push", not(windows)))]
async fn run_stream(_command: StreamCommand) -> Result<()> {
    bail!("Windows Terminal capture is available only on Windows")
}
