mod client;
mod config;
mod delta;
mod index;
mod patterns;
mod proto;
mod rewrite;
mod server;
mod shim;
mod xfer;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;

/// Remote builds: sync the project to another machine over ssh, run the command there, pull the outputs back.
///
/// `mirako ./gradlew assembleDebug` is the same as `mirako run -- ./gradlew assembleDebug`.
#[derive(Parser)]
#[command(name = "mirako", version, about, allow_external_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// push → run COMMAND on the remote → pull
    Run {
        #[arg(long, short)]
        project: Option<PathBuf>,
        #[arg(long)]
        host: Option<String>,
        /// skip the upload
        #[arg(long)]
        no_push: bool,
        /// skip the download
        #[arg(long)]
        no_pull: bool,
        #[arg(long, short)]
        quiet: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// upload the project (sources) to the remote
    Push {
        #[arg(long, short)]
        project: Option<PathBuf>,
        #[arg(long)]
        host: Option<String>,
    },
    /// download the build outputs from the remote
    Pull {
        #[arg(long, short)]
        project: Option<PathBuf>,
        #[arg(long)]
        host: Option<String>,
    },
    /// handshake with the remote agent (exit 0 when usable)
    Check {
        #[arg(long, short)]
        project: Option<PathBuf>,
        #[arg(long)]
        host: Option<String>,
    },
    /// copy this binary to the remote (`remote_bin`, default ~/.local/bin/mirako)
    RemoteInstall {
        #[arg(long, short)]
        project: Option<PathBuf>,
        #[arg(long)]
        host: Option<String>,
    },
    /// write sample config files (global and/or per-project)
    Init {
        /// write <project>/mirako.toml
        #[arg(long, short)]
        project: Option<PathBuf>,
        /// write ~/.config/mirako/config.toml
        #[arg(long)]
        global: bool,
    },
    /// the Gradle init script for Android Studio / ./gradlew
    GradleShim {
        #[command(subcommand)]
        what: ShimCmd,
    },
    /// the agent started over ssh by the client (not for humans)
    Serve,
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum ShimCmd {
    /// write ~/.gradle/init.d/mirako.gradle pointing at this binary
    Install,
    /// print the init script
    Print,
}

fn main() {
    let code = match real_main() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mirako: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn load(project: Option<&PathBuf>, host: Option<&str>) -> Result<(PathBuf, Config)> {
    let root = client::project_root_for(project)?;
    let cfg = Config::load(&root, host)?;
    Ok((root, cfg))
}

fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            Ok(2)
        }
        Some(Cmd::Serve) => {
            server::serve()?;
            Ok(0)
        }
        Some(Cmd::External(command)) => {
            let (root, cfg) = load(None, None)?;
            client::run(
                &root,
                &cfg,
                &command,
                &client::RunOptions {
                    push: true,
                    pull: true,
                    quiet: false,
                },
            )
        }
        Some(Cmd::Run {
            project,
            host,
            no_push,
            no_pull,
            quiet,
            command,
        }) => {
            let (root, cfg) = load(project.as_ref(), host.as_deref())?;
            client::run(
                &root,
                &cfg,
                &command,
                &client::RunOptions {
                    push: !no_push,
                    pull: !no_pull,
                    quiet,
                },
            )
        }
        Some(Cmd::Push { project, host }) => {
            let (root, cfg) = load(project.as_ref(), host.as_deref())?;
            client::run(
                &root,
                &cfg,
                &[],
                &client::RunOptions {
                    push: true,
                    pull: false,
                    quiet: false,
                },
            )
        }
        Some(Cmd::Pull { project, host }) => {
            let (root, cfg) = load(project.as_ref(), host.as_deref())?;
            client::run(
                &root,
                &cfg,
                &[],
                &client::RunOptions {
                    push: false,
                    pull: true,
                    quiet: false,
                },
            )
        }
        Some(Cmd::Check { project, host }) => {
            let (_root, cfg) = load(project.as_ref(), host.as_deref())?;
            client::check(&cfg)?;
            Ok(0)
        }
        Some(Cmd::RemoteInstall { project, host }) => {
            let (_root, cfg) = load(project.as_ref(), host.as_deref())?;
            client::remote_install(&cfg)?;
            Ok(0)
        }
        Some(Cmd::Init { project, global }) => {
            if global {
                let p = config::global_config_path();
                if p.exists() {
                    println!("{} already exists", p.display());
                } else {
                    std::fs::create_dir_all(p.parent().unwrap())?;
                    std::fs::write(&p, config::SAMPLE_GLOBAL_TOML)?;
                    println!("wrote {}", p.display());
                }
            }
            if project.is_some() || !global {
                let root = client::project_root_for(project.as_ref())?;
                let p = root.join("mirako.toml");
                if p.exists() {
                    println!("{} already exists", p.display());
                } else {
                    std::fs::write(&p, config::SAMPLE_PROJECT_TOML)?;
                    println!("wrote {}", p.display());
                }
            }
            Ok(0)
        }
        Some(Cmd::GradleShim { what }) => {
            match what {
                ShimCmd::Install => println!("wrote {}", shim::install()?.display()),
                ShimCmd::Print => print!("{}", shim::init_script(&std::env::current_exe()?.to_string_lossy())),
            }
            Ok(0)
        }
    }
}
