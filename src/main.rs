mod api;
mod controller;
mod manifests;

use clap::{Parser, Subcommand};
use kube::{Client, CustomResourceExt};
use tracing_subscriber::{EnvFilter, fmt};

use crate::api::NervixCluster;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the NervixCluster CustomResourceDefinition as YAML.
    Crd,
    /// Run the Kubernetes controller.
    Run,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run) {
        Command::Crd => {
            println!("{}", serde_yaml::to_string(&NervixCluster::crd())?);
        }
        Command::Run => {
            fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new("info,nervix_k8s_operator=debug")),
                )
                .init();

            let client = Client::try_default().await?;
            controller::run(client).await?;
        }
    }

    Ok(())
}
