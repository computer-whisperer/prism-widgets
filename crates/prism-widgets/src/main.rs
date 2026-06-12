mod config;

use anyhow::Result;
use prism_widgets_host::{run_layer_shell, HostConfig, PanelRunner};
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
    let host_config = HostConfig {
        panels: config.panel_specs(),
    };
    let store = SnapshotStore::from_specs(&host_config.panels);
    let dry_run = std::env::args().any(|arg| arg == "--dry-run");
    if !dry_run {
        return run_layer_shell(host_config, Box::new(store));
    }

    let runner = PanelRunner::new(host_config);
    for snapshot in runner.snapshots(&store) {
        println!("panel {}", snapshot.panel_id.0);
        for module in snapshot.modules {
            println!("  {}: {:?}", module.id, module.value);
        }
    }

    Ok(())
}
