use anyhow::{Context, Result};
use app::{AppModule, AppModuleCtx};
use axum::Router;
use clap::Parser;
use client_sdk::rest_client::{IndexerApiHttpClient, NodeApiHttpClient};
use hyle::{
    bus::{metrics::BusMetrics, SharedMessageBus},
    indexer::da_listener::{DAListener, DAListenerCtx},
    model::{api::NodeInfo, CommonRunContext},
    rest::{RestApi, RestApiRunContext},
    utils::{conf, logger::setup_tracing, modules::ModulesHandler},
};
use prometheus::Registry;
use prover::{ProverModule, ProverModuleCtx};
use std::{
    env,
    sync::{Arc, Mutex},
};
use tracing::{error, info, warn};

mod app;
mod init;
mod prover;
mod utils;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[arg(long, default_value = "config.toml")]
    pub config_file: Option<String>,

    #[arg(long, default_value = "blackjack")]
    pub contract_name: String,

    #[clap(long, action)]
    pub pg: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config =
        conf::Conf::new(args.config_file, None, Some(true)).context("reading config file")?;

    setup_tracing(&config, format!("{}(nopkey)", config.id.clone(),))
        .context("setting up tracing")?;

    let config = Arc::new(config);

    info!("Starting app with config: {:?}", &config);

    let node_url = env::var("NODE_URL").unwrap_or_else(|_| "http://localhost:4321".to_string());
    let indexer_url =
        env::var("INDEXER_URL").unwrap_or_else(|_| "http://localhost:4321".to_string());
    let node_client = Arc::new(NodeApiHttpClient::new(node_url).context("build node client")?);
    let indexer_client =
        Arc::new(IndexerApiHttpClient::new(indexer_url).context("build indexer client")?);

    match init::init_node(
        node_client.clone(),
        indexer_client.clone(),
        args.contract_name.clone(),
    )
    .await
    {
        Ok(_) => {}
        Err(e) => {
            error!("Error initializing node: {:?}", e);
            return Ok(());
        }
    }

    let bus = SharedMessageBus::new(BusMetrics::global(config.id.clone()));

    std::fs::create_dir_all(&config.data_directory).context("creating data directory")?;

    let mut handler = ModulesHandler::new(&bus).await;

    let ctx = Arc::new(CommonRunContext {
        bus: bus.new_handle(),
        config: config.clone(),
        router: Mutex::new(Some(Router::new())),
        openapi: Default::default(),
    });

    let app_ctx = Arc::new(AppModuleCtx {
        common: ctx.clone(),
        node_client,
        indexer_client,
        blackjack_cn: args.contract_name.into(),
    });
    let start_height = app_ctx.node_client.get_block_height().await?;
    let prover_ctx = Arc::new(ProverModuleCtx {
        app: app_ctx.clone(),
        start_height,
    });

    handler.build_module::<AppModule>(app_ctx.clone()).await?;

    handler
        .build_module::<ProverModule>(prover_ctx.clone())
        .await?;

    handler
        .build_module::<DAListener>(DAListenerCtx {
            common: ctx.clone(),
            start_block: None,
        })
        .await?;

    // Should come last so the other modules have nested their own routes.
    #[allow(clippy::expect_used, reason = "Fail on misconfiguration")]
    let router = ctx
        .router
        .lock()
        .expect("Context router should be available")
        .take()
        .expect("Context router should be available");

    handler
        .build_module::<RestApi>(RestApiRunContext {
            port: ctx.config.rest_server_port,
            max_body_size: ctx.config.rest_server_max_body_size,
            bus: ctx.bus.new_handle(),
            metrics_layer: None,
            registry: Registry::new(),
            router: router.clone(),
            openapi: Default::default(),
            info: NodeInfo {
                id: ctx.config.id.clone(),
                da_address: ctx.config.da_address.clone(),
                pubkey: None,
            },
        })
        .await?;

    #[cfg(unix)]
    {
        use tokio::signal::unix;
        let mut terminate = unix::signal(unix::SignalKind::interrupt())?;
        tokio::select! {
            Err(e) = handler.start_modules() => {
                error!("Error running modules: {:?}", e);
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down");
            }
            _ = terminate.recv() =>  {
                info!("SIGTERM received, shutting down");
            }
        }
        _ = handler.shutdown_modules().await;
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            Err(e) = handler.start_modules() => {
                error!("Error running modules: {:?}", e);
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down");
            }
        }
        _ = handler.shutdown_modules().await;
    }

    if args.pg {
        warn!("--pg option given. Postgres server will stop. Cleaning data dir");
        std::fs::remove_dir_all(&config.data_directory).context("removing data directory")?;
    }

    Ok(())
}
