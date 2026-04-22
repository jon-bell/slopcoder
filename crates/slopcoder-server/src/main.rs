mod routes;
mod state;

use std::io::{self, Write};
use std::net::SocketAddr;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use uuid::Uuid;
use warp::Filter;

use state::AppState;

const DEFAULT_LIST_REQUEST_TIMEOUT_SECS: u64 = 15;

struct ServerCli {
    addr_arg: Option<String>,
    static_dir_arg: Option<std::path::PathBuf>,
    ui_password_prompt: bool,
    agent_password_prompt: bool,
    no_password: bool,
    explicit_ui_password: Option<String>,
    explicit_agent_password: Option<String>,
    list_request_timeout_secs: u64,
    dev_mode: bool,
}

fn parse_cli_args<I>(args: I) -> ServerCli
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut cli = ServerCli {
        addr_arg: None,
        static_dir_arg: None,
        ui_password_prompt: false,
        agent_password_prompt: false,
        no_password: false,
        explicit_ui_password: None,
        explicit_agent_password: None,
        list_request_timeout_secs: DEFAULT_LIST_REQUEST_TIMEOUT_SECS,
        dev_mode: std::env::var("SLOPCODER_DEV_MODE").ok().map_or(false, |v| v == "1"),
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "--bind" => {
                cli.addr_arg = args.next();
            }
            "--static-dir" | "--assets" => {
                cli.static_dir_arg = args.next().map(std::path::PathBuf::from);
            }
            "--password-prompt" => {
                cli.ui_password_prompt = true;
            }
            "--password" => {
                cli.explicit_ui_password = args.next();
            }
            "--agent-password-prompt" => {
                cli.agent_password_prompt = true;
            }
            "--agent-password" => {
                cli.explicit_agent_password = args.next();
            }
            "--no-password" => {
                cli.no_password = true;
            }
            "--list-request-timeout-secs" => {
                cli.list_request_timeout_secs = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or(DEFAULT_LIST_REQUEST_TIMEOUT_SECS);
            }
            "--dev-mode" => {
                cli.dev_mode = true;
            }
            "-h" | "--help" => {
                println!(
                    "Usage: slopcoder-server [--addr HOST:PORT] [--static-dir PATH] [--password VALUE|--password-prompt|--no-password] [--agent-password VALUE|--agent-password-prompt] [--list-request-timeout-secs SECONDS] [--dev-mode]\n\
Defaults: addr=127.0.0.1:8080, static-dir=frontend/dist, UI auth disabled, agent auth enabled with generated startup password, list-request-timeout-secs=15\n\
Dev mode: --dev-mode or SLOPCODER_DEV_MODE=1 enables /auth/dev-login for testing"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    cli
}

#[tokio::main]
async fn main() {
    // Initialize logging
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("slopcoder=info".parse().unwrap()))
        .init();

    let cli = parse_cli_args(std::env::args().skip(1));

    let ui_auth_password = if cli.no_password {
        tracing::warn!("UI authentication disabled (--no-password).");
        None
    } else if let Some(password) = cli.explicit_ui_password {
        println!("UI password: {}", password);
        Some(password)
    } else if cli.ui_password_prompt {
        print!("Enter UI password: ");
        io::stdout().flush().expect("Failed to flush stdout");
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .expect("Failed to read password");
        let trimmed = input.trim_end_matches(&['\r', '\n'][..]).to_string();
        if trimmed.is_empty() {
            None
        } else {
            println!("UI password: {}", trimmed);
            Some(trimmed)
        }
    } else {
        None
    };

    let agent_auth_password = if let Some(password) = cli.explicit_agent_password {
        password
    } else if cli.agent_password_prompt {
        print!("Enter slopagent connection password: ");
        io::stdout().flush().expect("Failed to flush stdout");
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .expect("Failed to read password");
        let trimmed = input.trim_end_matches(&['\r', '\n'][..]).to_string();
        if trimmed.is_empty() {
            tracing::error!("Agent password cannot be empty");
            std::process::exit(1);
        }
        trimmed
    } else {
        Uuid::new_v4().simple().to_string()[..16].to_string()
    };
    println!("Slopagent password: {}", agent_auth_password);

    let state = AppState::new(
        ui_auth_password,
        agent_auth_password,
        cli.list_request_timeout_secs,
        cli.dev_mode,
    );

    // Build API routes
    let api_routes = routes::routes(state);

    // Add CORS for development
    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE", "OPTIONS"])
        .allow_headers(vec!["Content-Type", "X-Slopcoder-Password"]);

    let api_routes = api_routes.with(cors);

    // Static file serving for frontend
    let static_dir = cli
        .static_dir_arg
        .or_else(|| {
            std::env::var("SLOPCODER_STATIC_DIR")
                .ok()
                .map(std::path::PathBuf::from)
        })
        .unwrap_or_else(|| std::path::PathBuf::from("frontend/dist"));

    let static_files = warp::fs::dir(static_dir.clone());

    // Serve index.html for SPA routes (fallback for client-side routing)
    let index_html = warp::fs::file(static_dir.join("index.html"));
    let spa_fallback = warp::any().and(warp::get()).and(index_html);

    // Combine: API routes first, then static files, then SPA fallback
    let routes = api_routes.or(static_files).or(spa_fallback);

    // Get address from args/env or use default (127.0.0.1:8080)
    let addr: SocketAddr = cli
        .addr_arg
        .or_else(|| std::env::var("SLOPCODER_ADDR").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| ([127, 0, 0, 1], 8080).into());

    tracing::info!("Starting server at http://{}", addr);

    warp::serve(routes).run(addr).await;
}

#[cfg(test)]
mod tests {
    use super::{parse_cli_args, DEFAULT_LIST_REQUEST_TIMEOUT_SECS};

    #[test]
    fn parse_cli_uses_default_list_request_timeout() {
        let cli = parse_cli_args(Vec::<String>::new());
        assert_eq!(
            cli.list_request_timeout_secs,
            DEFAULT_LIST_REQUEST_TIMEOUT_SECS
        );
    }

    #[test]
    fn parse_cli_accepts_list_request_timeout_override() {
        let cli = parse_cli_args(vec![
            "--list-request-timeout-secs".to_string(),
            "22".to_string(),
        ]);
        assert_eq!(cli.list_request_timeout_secs, 22);
    }
}
