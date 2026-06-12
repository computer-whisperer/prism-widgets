mod config;

use anyhow::Result;
use prism_widgets_host::{run_layer_shell_with_reload, ConfigReloader, HostConfig, PanelRunner};
use prism_widgets_providers::SnapshotStore;

use crate::config::Config;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "prism_widgets=info".into()),
        )
        .init();

    let config = Config::load()?;
    let initial_host_config = build_host_config(&config);
    let store = SnapshotStore::from_specs(&initial_host_config.panels);
    let dry_run = std::env::args().any(|arg| arg == "--dry-run");
    if !dry_run {
        let reloader = Config::path().map(|path| {
            ConfigReloader::new(path, || {
                let config = Config::load()?;
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

fn build_host_config(config: &Config) -> HostConfig {
    HostConfig {
        panels: config.panel_specs(),
    }
}
