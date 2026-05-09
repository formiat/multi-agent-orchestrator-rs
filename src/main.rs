mod constants;
mod errors;
mod fsm;
mod locks;
mod orchestrator;
mod providers;
mod report;
mod sessions;
mod signals;
mod state;
mod transport;
mod workflow;
mod yaml_check;

use std::path::PathBuf;

use clap::Parser;

use crate::orchestrator::CliArgs;
use crate::state::{NotionPolicy, ProviderKind, RemoteNetworkPolicy, WorkflowType};

/// Deterministic multi-agent orchestrator for delegated coding workflows.
#[derive(Parser, Debug)]
#[command(name = "orchestrate", version, about)]
struct Cli {
    /// Workflow type: plan | investigate | implement
    #[arg(short = 'w', long)]
    workflow: WorkflowType,

    /// Path to the target workspace root (relative paths, symlinks, and `..` are resolved automatically)
    #[arg(short = 'r', long)]
    workspace_root: PathBuf,

    /// Executor session title — must match the thread name in the executor provider
    #[arg(long)]
    executor_thread_name: String,

    /// Reviewer session title — must match the thread name in the reviewer provider
    #[arg(long)]
    reviewer_thread_name: String,

    /// Initial prompt / task description delivered to the executor
    #[arg(short = 'p', long)]
    prompt: String,

    /// Notion task policy: required | optional
    #[arg(long, default_value = "optional")]
    notion_policy: NotionPolicy,

    /// Remote network policy for external production systems: forbidden | allowed
    #[arg(long, default_value = "forbidden")]
    remote_network_policy: RemoteNetworkPolicy,

    /// Executor provider: claude | opencode | codex
    #[arg(short = 'e', long)]
    executor_provider: ProviderKind,

    /// Reviewer provider: claude | opencode | codex
    #[arg(long)]
    reviewer_provider: ProviderKind,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let args = CliArgs {
        workflow_type: cli.workflow,
        workspace_root: cli.workspace_root,
        executor_thread_name: cli.executor_thread_name,
        reviewer_thread_name: cli.reviewer_thread_name,
        prompt: cli.prompt,
        notion_policy: cli.notion_policy,
        remote_network_policy: cli.remote_network_policy,
        executor_provider: cli.executor_provider,
        reviewer_provider: cli.reviewer_provider,
    };

    let report = orchestrator::run(args).await;
    let success = report.reason_code.is_success();
    report.print();
    if !success {
        std::process::exit(1);
    }
}
