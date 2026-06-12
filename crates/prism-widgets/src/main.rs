mod config;

use std::path::PathBuf;

use anyhow::{Context, Result};
use prism_widgets_host::{run_layer_shell_with_reload, ConfigReloader, HostConfig, PanelRunner};
use prism_widgets_providers::SnapshotStore;

use crate::config::Config;

const DEFAULT_CONFIG: &str = include_str!("../../../resources/default-config.kdl");

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "prism_widgets=info".into()),
        )
        .init();

    let args = Args::parse()?;
    if args.help {
        print_help();
        return Ok(());
    }
    if args.dump_default_config {
        print!("{DEFAULT_CONFIG}");
        return Ok(());
    }

    let config_path = args.config_path.or_else(Config::path);
    if args.print_config_path {
        match &config_path {
            Some(path) => println!("{}", path.display()),
            None => println!("unavailable: HOME is not set"),
        }
        return Ok(());
    }
    if args.init_config {
        init_config(config_path.as_ref().context("HOME is not set")?)?;
        return Ok(());
    }

    let config = load_config(config_path.as_ref())?;
    let initial_host_config = build_host_config(&config);
    let store = SnapshotStore::from_specs(&initial_host_config.panels);
    if !args.dry_run {
        let reloader = config_path.map(|path| {
            ConfigReloader::new(path.clone(), move || {
                let config = Config::load_from_path(&path)?;
                let host_config = build_host_config(&config);
                let store = SnapshotStore::from_specs(&host_config.panels);
                Ok((host_config, Box::new(store)))
            })
        });
        return run_layer_shell_with_reload(initial_host_config, Box::new(store), reloader);
    }

    let runner = PanelRunner::new(initial_host_config);
    for snapshot in runner.snapshots(&store) {
        println!("panel {}", snapshot.panel_id.0);
        for module in snapshot.modules {
            println!("  {}: {:?}", module.id, module.value);
        }
    }

    Ok(())
}

#[derive(Debug, Default)]
struct Args {
    config_path: Option<PathBuf>,
    dry_run: bool,
    dump_default_config: bool,
    help: bool,
    init_config: bool,
    print_config_path: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut parsed = Self::default();
        let mut args = std::env::args_os().skip(1);
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--config") | Some("-c") => {
                    let path = args.next().context("--config requires a path argument")?;
                    parsed.config_path = Some(PathBuf::from(path));
                }
                Some("--dry-run") => parsed.dry_run = true,
                Some("--dump-default-config") => parsed.dump_default_config = true,
                Some("--help") | Some("-h") => parsed.help = true,
                Some("--init-config") => parsed.init_config = true,
                Some("--print-config-path") => parsed.print_config_path = true,
                Some(other) => anyhow::bail!("unknown argument {other:?}; pass --help"),
                None => anyhow::bail!("non-UTF-8 argument {arg:?}"),
            }
        }
        Ok(parsed)
    }
}

fn load_config(path: Option<&PathBuf>) -> Result<Config> {
    match path {
        Some(path) => Config::load_from_path(path),
        None => Config::load(),
    }
}

fn init_config(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        anyhow::bail!("{} already exists", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, DEFAULT_CONFIG).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn build_host_config(config: &Config) -> HostConfig {
    HostConfig {
        panels: config.panel_specs(),
    }
}

fn print_help() {
    println!(
        "\
prism-widgets

Usage:
  prism-widgets [OPTIONS]

Options:
  -c, --config PATH       Read config from PATH instead of PRISM_WIDGETS_CONFIG/XDG
      --dry-run           Print one snapshot and exit
      --dump-default-config
                          Print the documented sample config
      --init-config       Write the sample config to the resolved config path
      --print-config-path Print the resolved config path and exit
  -h, --help              Show this help
"
    );
}
