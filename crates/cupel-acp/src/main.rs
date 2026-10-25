//! Cupel's ACP v1 stdio frontend. stdout belongs exclusively to the protocol.

mod config;
mod events;
mod server;
mod session;

use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio};

use crate::server::Server;
use crate::session::Notify;

fn notifier(connection: ConnectionTo<Client>, id: acp::SessionId) -> Notify {
    Arc::new(move |update| {
        connection.send_notification(acp::SessionNotification::new(id.clone(), update))
    })
}

async fn interrupted() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

async fn run(config: config::Config) -> acp::Result<()> {
    let server = Server::new(config);
    let initialize = Arc::clone(&server);
    let create = Arc::clone(&server);
    let load = Arc::clone(&server);
    let prompt = Arc::clone(&server);
    let cancel = Arc::clone(&server);
    let connection = Agent
        .builder()
        .name("cupel-acp")
        .on_receive_request(
            async move |_request: acp::InitializeRequest, responder, _cx| {
                responder.respond_with_result(initialize.initialized())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::NewSessionRequest, responder, cx| {
                let server = Arc::clone(&create);
                let id = match Server::fresh_id() {
                    Ok(id) => id,
                    Err(error) => return responder.respond_with_error(error),
                };
                let notify = notifier(cx.clone(), acp::SessionId::new(id.clone()));
                create.spawn(async move {
                    let result = server
                        .create(&id, request.cwd, &request.mcp_servers, false, notify)
                        .await;
                    responder.respond_with_result(result.map(|()| acp::NewSessionResponse::new(id)))
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::LoadSessionRequest, responder, cx| {
                let server = Arc::clone(&load);
                let notify = notifier(cx.clone(), request.session_id.clone());
                load.spawn(async move {
                    let result = server
                        .create(
                            &request.session_id.to_string(),
                            request.cwd,
                            &request.mcp_servers,
                            true,
                            notify,
                        )
                        .await;
                    responder.respond_with_result(result.map(|()| acp::LoadSessionResponse::new()))
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::PromptRequest, responder, cx| {
                let server = Arc::clone(&prompt);
                let notify = notifier(cx.clone(), request.session_id.clone());
                let turn = match prompt.begin_prompt(request) {
                    Ok(turn) => turn,
                    Err(error) => return responder.respond_with_error(error),
                };
                prompt.spawn(async move {
                    responder.respond_with_result(server.finish_prompt(turn, notify).await)
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |request: acp::CancelNotification, _cx| {
                cancel.cancel(&request.session_id);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new());
    let result = tokio::select! {
        result = connection => result,
        signal = interrupted() => signal.map_err(acp::Error::into_internal_error),
    };
    server.shutdown().await?;
    result
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let result = match config::parse(std::env::args().skip(1)) {
        Ok(Some(config)) => run(config).await,
        Ok(None) => {
            eprint!("{}", config::HELP);
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => Err(session::invalid(error)),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cupel-acp: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
