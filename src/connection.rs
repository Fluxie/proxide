use bytes::Bytes;
use chrono::prelude::*;
use futures::{join, prelude::*};
use h2::{
    client::{self, ResponseFuture},
    server::{self, SendResponse},
    Reason, RecvStream, SendStream,
};
use http::{HeaderMap, Request, Response};
use log::error;
use snafu::{ResultExt, Snafu};
use std::net::SocketAddr;
use std::sync::mpsc::Sender;
use tokio::net::TcpStream;
use uuid::Uuid;

use crate::session::events::*;
use crate::session::*;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error
{
    #[snafu(display("HTTP error occurred with the server in {}: {}", scenario, source))]
    ServerError
    {
        scenario: &'static str,
        source: h2::Error,
    },

    #[snafu(display("HTTP error occurred with the client in {}: {}", scenario, source))]
    ClientError
    {
        scenario: &'static str,
        source: h2::Error,
    },
}

pub type Result<S, E = Error> = std::result::Result<S, E>;

pub struct ProxyConnection
{
    uuid: Uuid,
    client_connection: server::Connection<TcpStream, Bytes>,
    server_stream: client::SendRequest<Bytes>,
}

impl ProxyConnection
{
    pub async fn new(
        client: TcpStream,
        server: TcpStream,
        src_addr: SocketAddr,
        ui: Sender<SessionEvent>,
    ) -> Result<ProxyConnection>
    {
        let client_connection = server::Builder::new()
            .initial_window_size(1_000_000)
            .handshake(client)
            .await
            .context(ClientError {
                scenario: "client handshake",
            })?;
        let (server_stream, server_connection) =
            client::handshake(server).await.context(ServerError {
                scenario: "server handshake",
            })?;

        tokio::spawn(async move {
            match server_connection.await {
                Ok(..) => {}
                Err(e) => error!("Error: {:?}", e),
            }
        });

        let server_stream = server_stream.ready().await.context(ServerError {
            scenario: "starting stream",
        })?;

        let conn = ProxyConnection {
            uuid: Uuid::new_v4(),
            client_connection,
            server_stream,
        };

        ui.send(SessionEvent::NewConnection(NewConnectionEvent {
            uuid: conn.uuid,
            client_addr: src_addr,
            timestamp: Local::now(),
        }))
        .unwrap();
        Ok(conn)
    }

    pub async fn run(&mut self, ui: Sender<SessionEvent>) -> Result<()>
    {
        let r = {
            let ui = ui.clone();
            let client_connection = &mut self.client_connection;
            let server_stream = &mut self.server_stream;
            let uuid = self.uuid;
            async move {
                while let Some(request) = client_connection.accept().await {
                    // Process the client request.
                    let (client_request, client_response) = request.context(ClientError {
                        scenario: "processing request",
                    })?;

                    let request = ProxyRequest::new(
                        uuid,
                        client_request,
                        client_response,
                        server_stream,
                        &ui,
                    )?;

                    let ui = ui.clone();
                    tokio::spawn(async move {
                        let ui = ui;
                        match request.execute(ui).await {
                            Ok(_) => {}
                            Err(e) => error!("{}", e),
                        }
                    });
                }

                Ok(())
            }
        }
        .await;

        ui.send(SessionEvent::ConnectionClosed {
            uuid: self.uuid,
            status: match r {
                Ok(_) => Status::Succeeded,
                Err(_) => Status::Failed,
            },
        })
        .unwrap();
        r
    }
}

pub struct ProxyRequest
{
    uuid: Uuid,
    connection_uuid: Uuid,
    client_request: RecvStream,
    client_response: SendResponse<Bytes>,
    server_request: SendStream<Bytes>,
    server_response: ResponseFuture,
}

impl ProxyRequest
{
    pub fn new(
        connection_uuid: Uuid,
        client_request: Request<RecvStream>,
        client_response: SendResponse<Bytes>,
        server_stream: &mut client::SendRequest<Bytes>,
        ui: &Sender<SessionEvent>,
    ) -> Result<ProxyRequest>
    {
        let uuid = Uuid::new_v4();
        let (client_head, client_request) = client_request.into_parts();

        ui.send(SessionEvent::NewRequest(NewRequestEvent {
            connection_uuid,
            uuid: uuid,
            uri: client_head.uri.clone(),
            method: client_head.method.clone(),
            headers: client_head.headers.clone(),
            timestamp: Local::now(),
        }))
        .unwrap();

        let server_request = Request::from_parts(client_head, ());

        // Set up a server request.
        let (server_response, server_request) = server_stream
            .send_request(server_request, false)
            .context(ServerError {
            scenario: "sending request",
        })?;

        Ok(ProxyRequest {
            uuid,
            connection_uuid,
            client_request,
            client_response,
            server_request,
            server_response,
        })
    }

    pub async fn execute(self, ui: Sender<SessionEvent>) -> Result<()>
    {
        // Acquire futures that are responsible for streaming the request and the response. These
        // are set up in their own futures to allow parallel request/response streaming to occur.

        // Set up streaming the request to the server.
        let uuid = self.uuid;
        let client_request = self.client_request;
        let mut server_request = self.server_request;
        let ui_temp = ui.clone();
        let request_future = async move {
            let ui = ui_temp;
            let trailers = pipe_stream(
                client_request,
                &mut server_request,
                ui,
                uuid,
                RequestPart::Request,
            )
            .await?;

            if let Some(trailers) = trailers.clone() {
                server_request
                    .send_trailers(trailers)
                    .context(ServerError {
                        scenario: "sending trailers",
                    })?;
            }
            Ok(trailers)
        }
        .then({
            let ui = ui.clone();
            move |r| notify_message_done(ui, uuid, r, RequestPart::Request)
        });

        // Set up streaming the response to the client.
        //
        // This is done in its own async block, since it's the pipe_stream async call that we'll
        // want to happen in parallel, but there's a good chance the server won't send the
        // response before the request stream has proceeded at least some. (Most likely the server
        // will require that stream to proceed in full, unless the call is some sort of a streaming
        // call.
        let mut client_response = self.client_response;
        let server_response = self.server_response;
        let connection_uuid = self.connection_uuid;
        let ui_temp = ui.clone();
        let response_future = async move {
            let ui = ui_temp;
            let response = server_response.await.context(ServerError {
                scenario: "waiting for response",
            })?;

            let (response_head, response_body) = response.into_parts();
            ui.send(SessionEvent::NewResponse(NewResponseEvent {
                uuid: uuid,
                connection_uuid,
                timestamp: Local::now(),
                headers: response_head.headers.clone(),
            }))
            .unwrap();

            let response = Response::from_parts(response_head, ());

            let mut client_stream = client_response
                .send_response(response, response_body.is_end_stream())
                .context(ClientError {
                    scenario: "sending response",
                })?;

            if !response_body.is_end_stream() {
                log::info!("{}: Server stream starting", uuid);
                let trailers = pipe_stream(
                    response_body,
                    &mut client_stream,
                    ui,
                    uuid,
                    RequestPart::Response,
                )
                .await?;
                log::info!("{}: Server stream ended", uuid);

                if let Some(trailers) = trailers.clone() {
                    client_stream.send_trailers(trailers).context(ServerError {
                        scenario: "sending trailers",
                    })?;
                }

                Ok(trailers)
            } else {
                Ok(None)
            }
        }
        .then({
            let ui = ui.clone();
            move |r| notify_message_done(ui, uuid, r, RequestPart::Response)
        });

        // Now handle both futures in parallel.
        let (r1, r2) = join!(request_future, response_future);
        let r = r1.and(r2);
        ui.send(SessionEvent::RequestDone(RequestDoneEvent {
            uuid: self.uuid,
            status: match fatal_error(&r) {
                true => Status::Failed,
                false => Status::Succeeded,
            },
            timestamp: Local::now(),
        }))
        .unwrap();
        r
    }
}

async fn pipe_stream(
    mut source: RecvStream,
    target: &mut SendStream<Bytes>,
    ui: Sender<SessionEvent>,
    uuid: Uuid,
    part: RequestPart,
) -> Result<Option<HeaderMap>>
{
    while let Some(data) = source.data().await {
        let b = data.context(ClientError {
            scenario: "reading content",
        })?;

        // Send a notification to the UI.
        ui.send(SessionEvent::MessageData(MessageDataEvent {
            uuid: uuid,
            data: b.clone(),
            part,
        }))
        .unwrap();

        let size = b.len();
        target
            .send_data(b, source.is_end_stream())
            .context(ServerError {
                scenario: "writing content",
            })?;
        source.flow_control().release_capacity(size).unwrap();
    }

    let t = source.trailers().await.context(ClientError {
        scenario: "receiving trailers",
    })?;
    Ok(t)
}

async fn notify_message_done(
    ui: Sender<SessionEvent>,
    uuid: Uuid,
    r: Result<Option<HeaderMap>>,
    part: RequestPart,
) -> Result<()>
{
    match r {
        Ok(trailers) => ui
            .send(SessionEvent::MessageDone(MessageDoneEvent {
                uuid: uuid,
                part,
                status: Status::Succeeded,
                timestamp: Local::now(),
                trailers: trailers,
            }))
            .unwrap(),
        Err(e) => {
            ui.send(SessionEvent::MessageDone(MessageDoneEvent {
                uuid: uuid,
                part,
                status: Status::Succeeded,
                timestamp: Local::now(),
                trailers: None,
            }))
            .unwrap();
            return Err(e);
        }
    }
    Ok(())
}

fn fatal_error<S>(r: &Result<S, Error>) -> bool
{
    match r {
        Ok(_) => false,
        Err(e) => match e {
            Error::ServerError { source, .. } | Error::ClientError { source, .. } => {
                match source.reason() {
                    Some(Reason::NO_ERROR) => false,
                    _ => true,
                }
            }
        },
    }
}