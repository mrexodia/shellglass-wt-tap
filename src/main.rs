#[cfg(all(feature = "push", windows))]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(feature = "push", all(feature = "serve", not(windows))))]
use anyhow::bail;
use clap::{Parser, Subcommand};
#[cfg(feature = "accessibility")]
use shellglass_wt_tap::accessibility::AccessibilityOptions;
#[cfg(any(feature = "accessibility", feature = "serve", feature = "push"))]
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "shellglass-wt-tap",
    version,
    about = "Native terminal and accessibility sources for shellglass"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[cfg(any(feature = "serve", feature = "push"))]
struct SourceOptions {
    keep_last_terminal: bool,
    #[cfg(feature = "accessibility")]
    accessibility: Option<AccessibilityOptions>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the active-window stream through the local shellglass viewer.
    #[cfg(feature = "serve")]
    Serve {
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        bind: String,
        #[arg(short, long, env = "SHELLGLASS_CONFIG")]
        config: Option<PathBuf>,
        /// Disable accessibility reconstruction and stop outside known terminals.
        #[arg(long)]
        foreground_only: bool,
        #[cfg(feature = "accessibility")]
        #[command(flatten)]
        accessibility: AccessibilityOptions,
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
    /// Push the active-window stream to a shellglass hub.
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
        /// Disable accessibility reconstruction and stop outside known terminals.
        #[arg(long)]
        foreground_only: bool,
        #[cfg(feature = "accessibility")]
        #[command(flatten)]
        accessibility: AccessibilityOptions,
        #[arg(long)]
        no_record: bool,
    },
    /// Control a detached hub stream worker.
    #[cfg(feature = "push")]
    Stream {
        #[command(subcommand)]
        command: StreamCommand,
    },
    /// Reconstruct the active accessibility window in this terminal.
    #[cfg(feature = "accessibility")]
    Preview {
        #[command(flatten)]
        accessibility: AccessibilityOptions,
    },
    /// Capture a screenshot/tree/TUI fixture for renderer development.
    #[cfg(feature = "accessibility")]
    CaptureLayoutFixture {
        output: PathBuf,
        /// Delay before capture so the target window can be focused.
        #[arg(long, default_value_t = 2_000)]
        delay_ms: u64,
        #[command(flatten)]
        accessibility: AccessibilityOptions,
    },
    /// Replay a captured accessibility tree through the current renderer.
    #[cfg(feature = "accessibility")]
    RenderLayoutFixture {
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
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
        /// Disable accessibility reconstruction and stop outside known terminals.
        #[arg(long)]
        foreground_only: bool,
        #[cfg(feature = "accessibility")]
        #[command(flatten)]
        accessibility: AccessibilityOptions,
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
            #[cfg(feature = "accessibility")]
            accessibility,
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
            options.source_label = if foreground_only {
                "the active native terminal"
            } else {
                "the active window (native terminal or accessibility reconstruction)"
            }
            .into();
            let source_options = SourceOptions {
                keep_last_terminal: !foreground_only,
                #[cfg(feature = "accessibility")]
                accessibility: (!foreground_only).then_some(accessibility),
            };
            shellglass::api::serve(move || start_source(source_options), presentation, options)
                .await
        }
        #[cfg(feature = "push")]
        Command::Push {
            url,
            key,
            config,
            foreground_only,
            #[cfg(feature = "accessibility")]
            accessibility,
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
            let source_options = SourceOptions {
                keep_last_terminal: !foreground_only,
                #[cfg(feature = "accessibility")]
                accessibility: (!foreground_only).then_some(accessibility),
            };
            shellglass::api::push(move || start_source(source_options), presentation, options).await
        }
        #[cfg(feature = "push")]
        Command::Stream { command } => run_stream(command).await,
        #[cfg(feature = "accessibility")]
        Command::Preview { accessibility } => {
            shellglass_wt_tap::accessibility::preview(accessibility).await
        }
        #[cfg(feature = "accessibility")]
        Command::CaptureLayoutFixture {
            output,
            delay_ms,
            accessibility,
        } => shellglass_wt_tap::accessibility::capture_layout_fixture(
            accessibility,
            &output,
            std::time::Duration::from_millis(delay_ms),
        ),
        #[cfg(feature = "accessibility")]
        Command::RenderLayoutFixture {
            input,
            output,
            cols,
            rows,
        } => shellglass_wt_tap::accessibility::render_layout_fixture(
            &input,
            output.as_deref(),
            cols,
            rows,
        ),
    }
}

#[cfg(any(feature = "serve", feature = "push"))]
fn start_source(options: SourceOptions) -> Result<shellglass::source::SourceSession> {
    #[cfg(all(windows, feature = "accessibility"))]
    {
        if let Some(accessibility) = options.accessibility {
            return shellglass_wt_tap::windows_native::start_hybrid(accessibility);
        }
        shellglass_wt_tap::windows_native::start(options.keep_last_terminal)
    }
    #[cfg(all(windows, not(feature = "accessibility")))]
    {
        shellglass_wt_tap::windows_native::start(options.keep_last_terminal)
    }
    #[cfg(all(not(windows), feature = "accessibility"))]
    {
        if let Some(accessibility) = options.accessibility {
            return shellglass_wt_tap::accessibility::start(accessibility);
        }
        let _ = options.keep_last_terminal;
        bail!("native terminal capture is available only on Windows")
    }
    #[cfg(all(not(windows), not(feature = "accessibility")))]
    {
        let _ = options.keep_last_terminal;
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
            #[cfg(feature = "accessibility")]
            accessibility,
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
            #[cfg(feature = "accessibility")]
            if !foreground_only {
                child
                    .arg("--a11y-interval-ms")
                    .arg(accessibility.interval_ms.to_string())
                    .arg("--a11y-cols")
                    .arg(accessibility.cols.to_string())
                    .arg("--a11y-rows")
                    .arg(accessibility.rows.to_string())
                    .arg("--a11y-depth")
                    .arg(accessibility.max_depth.to_string())
                    .arg("--a11y-max-nodes")
                    .arg(accessibility.max_nodes.to_string());
                if let Some(config) = &accessibility.policy_config {
                    child.arg("--a11y-config").arg(config);
                }
                for app in &accessibility.denied_apps {
                    child.arg("--a11y-deny-app").arg(app);
                }
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
