mod args;
mod bench_cmd;
mod context_cli;
mod http;
mod init;
mod requests;
mod serve_cmd;

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use freellama::{
    platform::{ExecutionPreference, PlacementEvidence, TaskKind},
    proxy::{ProxyConfig, serve},
};
use serde_json::{Value, json};

use args::{Cli, Command, PlatformStartArgs, TaskExecutionArgs};
use bench_cmd::benchmark;
use freellama::serve_client;
use http::{print_get, print_post, print_response};
use init::{generate_auth_token, initialize, print_doctor};
use requests::{request_route, request_task};
use serve_cmd::start_platform;

/// Print one line to stdout. A closed reader surfaces as an `io::ErrorKind::BrokenPipe` error,
/// which `main` turns into a quiet exit instead of `println!`'s panic.
pub(crate) fn emit(text: impl std::fmt::Display) -> Result<()> {
    writeln!(std::io::stdout().lock(), "{text}")?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        // `freellama status | head` closing the pipe early is not a failure.
        Err(error) if is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            ExitCode::FAILURE
        }
    }
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "the exhaustive CLI dispatch stays visible in one place"
)]
async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init {
            ollama_endpoint,
            serve_endpoint,
        } => initialize(ollama_endpoint, serve_endpoint).await?,
        Command::AuthToken { out } => generate_auth_token(&out)?,
        Command::Serve {
            listen,
            backends,
            benchmark_report,
            policy_file,
            admission,
            production,
        } => {
            start_platform(PlatformStartArgs {
                listen,
                backends,
                benchmark_report,
                policy_file,
                admission,
                production,
            })
            .await?;
        }
        Command::Models { endpoint } => {
            print_get(&endpoint, "/_freellama/v1/models").await?;
        }
        Command::Machine { endpoint } => {
            print_get(&endpoint, "/_freellama/v1/machine").await?;
        }
        Command::Scope { endpoint, action } => context_cli::run_scope(&endpoint, action).await?,
        Command::Warm(args) => context_cli::run_warm(args).await?,
        Command::Session { endpoint } => {
            print_post(&endpoint, "/_freellama/v1/sessions", &json!({})).await?;
        }
        Command::Status { endpoint } => {
            print_get(&endpoint, "/_freellama/v1/status").await?;
        }
        Command::Usage { endpoint, days } => {
            print_get(&endpoint, &format!("/_freellama/v1/usage?days={days}")).await?;
        }
        Command::Config { endpoint, reload } => {
            if reload {
                print_post(&endpoint, "/_freellama/v1/config/reload", &json!({})).await?;
            } else {
                print_get(&endpoint, "/_freellama/v1/config").await?;
            }
        }
        Command::Route(args) => request_route(args).await?,
        Command::Task(args) => request_task(args).await?,
        Command::Jobs {
            endpoint,
            id,
            cancel,
            remove,
        } => match (id, cancel, remove) {
            (Some(id), _, true) => {
                let response = serve_client::authenticate(serve_client::client().delete(
                    freellama::endpoint_url(&endpoint, &format!("/_freellama/v1/jobs/{id}")),
                ))?
                .timeout(serve_client::task_timeout())
                .send()
                .await?;
                print_response(response).await?;
            }
            (Some(id), true, false) => {
                print_post(
                    &endpoint,
                    &format!("/_freellama/v1/jobs/{id}/cancel"),
                    &Value::Null,
                )
                .await?;
            }
            (Some(id), false, false) => {
                print_get(&endpoint, &format!("/_freellama/v1/jobs/{id}")).await?;
            }
            (None, _, _) => print_get(&endpoint, "/_freellama/v1/jobs").await?,
        },
        Command::Proxy {
            listen,
            upstream,
            resource_telemetry_policy,
            allow_remote,
            request_timeout_seconds,
            auto_restart_ollama,
            max_concurrent_requests,
        } => {
            let mut config = ProxyConfig::new(listen, upstream, allow_remote)
                .with_request_timeout(std::time::Duration::from_secs(request_timeout_seconds))
                .with_auto_restart_ollama(auto_restart_ollama);
            if let Some(max) = max_concurrent_requests {
                config = config.with_max_concurrent_requests(max);
            }
            if let Some(value) = resource_telemetry_policy {
                let policy = freellama::platform::resources::ResourcePolicy {
                    telemetry_policy: value.into(),
                    ..freellama::platform::resources::ResourcePolicy::default()
                };
                config = config.with_resource_governor(
                    freellama::platform::resources::ResourceGovernor::new(policy)
                        .map_err(anyhow::Error::msg)?,
                );
            }
            serve(config).await?;
        }
        Command::BenchAll {
            endpoint,
            include,
            timeout_seconds,
            trials,
            output,
        } => benchmark(endpoint, include, timeout_seconds, trials, output).await?,
        Command::Doctor { endpoint } => print_doctor(&endpoint).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_broken_pipe;
    use anyhow::Context as _;
    use std::io::{Error, ErrorKind};

    #[test]
    fn broken_pipe_is_found_through_context_layers() {
        let error = Err::<(), _>(Error::from(ErrorKind::BrokenPipe))
            .context("print response")
            .unwrap_err();
        assert!(is_broken_pipe(&error));
    }

    #[test]
    fn other_io_errors_still_fail() {
        let error = anyhow::Error::from(Error::from(ErrorKind::PermissionDenied));
        assert!(!is_broken_pipe(&error));
    }
}
