use clap::Parser;

use cli::{Cli, Command};
use commands::Env;

/// `println!` that stops quietly when the reader has gone away, so
/// `precip ... | head` ends like any other command instead of panicking.
macro_rules! out {
    ($($arg:tt)*) => {
        print_line(format_args!($($arg)*))
    };
}

pub(crate) fn print_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let written = stdout.write_fmt(args).and_then(|()| stdout.write_all(b"\n"));
    if written.is_err_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
        std::process::exit(0);
    }
}

/// mimalloc in secure mode for the whole binary: guard pages, randomised
/// allocation and double-free detection, at a small speed cost.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cli;
mod commands;
mod schedule;

/// Logs to stderr. `RUST_LOG` selects levels; the default shows precip at
/// info and hides the cloud SDKs' own chatter.
pub(crate) fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "info,aws=warn,hyper=warn,azure_core=warn,azure_identity=warn,typespec_client_core=warn,google_cloud_auth=warn",
            )
        });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let env = Env::new(&cli);
    let args = &cli.scan;
    match cli.command {
        None => commands::tui(env, args).await,
        Some(Command::Scan) => commands::scan(&env, args).await,
        Some(Command::Group) => commands::group(&env).await,
        Some(Command::Merge { alias, into }) => {
            commands::merge(&env, &alias, &into).await
        }
        Some(Command::Disposition { pair, disposition, note }) => {
            commands::disposition(&env, &pair, disposition, note.as_deref())
                .await
        }
        Some(Command::DefaultDisposition { disposition, apply }) => {
            commands::default_disposition(&env, disposition, apply).await
        }
        Some(Command::Workspaces) => commands::workspaces(&env).await,
        Some(Command::Overview) => commands::overview(&env).await,
        Some(Command::Scans) => commands::scans(&env).await,
        Some(Command::Accounts) => commands::accounts(&env).await,
        Some(Command::Workloads) => commands::workloads(&env).await,
        Some(Command::Members { pair }) => commands::members(&env, &pair).await,
        Some(Command::History { pair }) => commands::history(&env, &pair).await,
        Some(Command::Size { sizing }) => commands::size(&env, &sizing).await,
        Some(Command::Prune { all }) => commands::prune(&env, all).await,
        Some(Command::Schedule { command }) => {
            let store = env.store("schedule").await?;
            schedule::run(&store, command).await
        }
        Some(Command::Export { what, out, format, sizing }) => {
            commands::export(&env, what, out, format, &sizing).await
        }
        Some(Command::Profiles { use_profile }) => {
            commands::profiles(&env, use_profile).await
        }
    }
}
