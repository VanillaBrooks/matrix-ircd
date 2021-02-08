// Copyright 2016 Openmarket
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The main module responsible for keeping track of the Matrix side of the world.
//!
//! It knows nothing about IRC.

use futures::prelude::Stream;
use futures::task::Poll;

use std::boxed::Box;
use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;
use std::task::Context;

use url::Url;

use rand::{thread_rng, Rng};

use crate::http;
use regex::Regex;

use quick_error::quick_error;

mod models;
pub mod protocol;
mod sync;

pub use self::models::{Member, Room};
use protocol::SyncResponse;

use crate::ConnectionContext;

use ruma_client::Client;
use ruma_client::api::r0 as api;
use ruma_client::identifiers;
use ruma_client::events;
use api::sync::sync_events;
type RumaClient = Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>;

#[derive(serde::Deserialize, serde::Serialize)]
struct TextMessage {
    msgtype: String,
    body: String,
}
impl TextMessage {
    fn new(text: String) -> Self {
        TextMessage {
            msgtype: "m.text".to_string(),
            body: text,
        }
    }
    fn raw_json(self) -> Result<Box<serde_json::value::RawValue>, Error> {
        let value = serde_json::value::to_raw_value(&self)?;
        Ok(value)
    }
}

/// A single Matrix session.
///
/// A `MatrixClient` both send requests and outputs a Stream of `SyncResponse`'s. It also keeps track
/// of vaious
pub struct MatrixClient {
    user_id: identifiers::UserId,
    client: RumaClient,
    rooms: BTreeMap<identifiers::RoomId, Room>,
    ctx: ConnectionContext,
    url: url::Url,
    stream: Option<Pin<Box<dyn Stream<Item = Result<sync_events::Response, ruma_client::Error<ruma_client::api::Error>>> + Send >>>
}

unsafe impl Sync for MatrixClient {}

impl MatrixClient {
    pub fn new(
        client: RumaClient,
        ctx: ConnectionContext,
        user_id: identifiers::UserId,
        url: url::Url
    ) -> MatrixClient {
        MatrixClient {
            client,
            rooms: BTreeMap::new(),
            ctx,
            user_id,
            url,
            stream: None
        }
    }

    /// The user ID associated with this session.
    pub fn get_user_id(&self) -> &identifiers::UserId {
        &self.user_id
    }

    /// Create a session by logging in with a user name and password.
    pub(crate) async fn login(
        base_url: Url,
        user: String,
        password: String,
        ctx: ConnectionContext,
    ) -> Result<MatrixClient, Error> {
        let http_client = http::ClientWrapper::new();
        let ruma_client = Client::https(base_url.clone(), None);

        let session = ruma_client.log_in(user, password, None, Some("matrix-ircd".to_string())).await?;

        let matrix_client = MatrixClient::new(
            ruma_client,
            ctx,
            session.identification.unwrap().user_id,
            base_url
        );
        Ok(matrix_client)
    }

    pub(crate) async fn send_text_message(
        &mut self,
        room_id: identifiers::RoomId,
        body: String,
    ) -> Result<api::message::create_message_event::Response, Error> {
        let txn_id= thread_rng().gen::<u16>().to_string();
        let event_type = events::EventType::RoomMessage;
        let data = TextMessage::new(body).raw_json()?;

        let request = api::message::create_message_event::Request {
            room_id,
            event_type,
            txn_id,
            data
        };

        let response = self.client.request(request).await?;

        Ok(response)
    }

    pub async fn join_room(&mut self, room_id: identifiers::RoomId) -> Result<api::membership::join_room_by_id::Response, Error> {
        let request = api::membership::join_room_by_id::Request {
            room_id,
            third_party_signed: None
        };
        let response = self.client.request(request).await?;
        Ok(response)
    }

    pub fn get_room(&self, room_id: &identifiers::RoomId) -> Option<&Room> {
        self.rooms.get(room_id)
    }

    pub fn media_url(&self, url: &str) -> String {
        lazy_static::lazy_static! {
            static ref RE: Regex = Regex::new("^mxc://([^/]+/[^/]+)$").unwrap();
        }
        if let Some(captures) = RE.captures(url) {
            self.url
                .join("/_matrix/media/v1/download/")
                .unwrap()
                .join(&captures[1])
                .unwrap()
                .into_string()
        } else {
            url.to_owned()
        }
    }

    fn poll_sync(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Option<Result<SyncResponse, Error>>> {

        if let Some(stream) = &mut self.stream {
            let resp = match stream.as_mut().poll_next(cx) {
                Poll::Ready(x) => x,
                Poll::Pending => return Poll::Pending
            };

            if let Some(sync_response) = resp {
                if let Ok(sync_response) = sync_response {

                    // map the serde sync_events::SyncResponse to protocol::SyncResponse so
                    // that we can use the existing fields.
                    let mut sync_response = protocol::SyncResponse::from_ruma(sync_response)?;

                    for (room_id, sync) in &mut sync_response.rooms.join {
                        sync.timeline.events.retain(|ev| {
                            !ev.unsigned
                                .transaction_id
                                .as_ref()
                                .map(|txn_id| txn_id.starts_with("mircd-"))
                                .unwrap_or(false)
                        });

                        if let Some(room) = self.rooms.get_mut(room_id) {

                            room.update_from_sync(sync);
                            continue;
                        }

                        // We can't put this in an else because of the mutable borrow in the if condition.
                        self.rooms
                            .insert(room_id.clone(), Room::from_sync(room_id.clone(), sync));
                    }

                    Poll::Ready(Some(Ok(sync_response)))
                }
                else {
                    unreachable!()
                }
            } 
                else {
                Poll::Ready(None)
            }

        }
        else {
            self.stream = Some(Box::pin(self.client.sync(None, None, api::sync::sync_events::SetPresence::Online, None)));
            self.poll_sync(cx)
        }

    }
}

quick_error! {
    #[derive(Debug)]
    enum JsonPostError {
        Io(err: io::Error) {
            from()
            description("io error")
            display("I/O error: {}", err)
            cause(err)
        }
        ErrorResponse(code: u16) {
            description("received non 200 response")
            display("Received response: {}", code)
        }
    }
}

impl JsonPostError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            JsonPostError::Io(err) => err,
            JsonPostError::ErrorResponse(code) => {
                io::Error::new(io::ErrorKind::Other, format!("Received {} response", code))
            }
        }
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: io::Error) {
            from()
            description("io error")
            display("I/O error: {}", err)
            cause(err)
        }
        InvalidPassword {
            description("password was invalid")
            display("Password is invalid")
        }
        HyperErr(err: hyper::Error) {
            from()
            description("hyper error when making http request")
            display("Hyper error: {}", err)
        }
        SerdeErr(err: serde_json::Error) {
            from()
            description("could not serialize / deserialize struct")
            display("Could not serialize request struct or deserialize response")
        }
        RumaClient(err: ruma_client::api::Error) {
            from()
        }
        RumaClientApi(err: ruma_client::Error<ruma_client::api::Error>) {
            from()
        }
    }
}

impl Stream for MatrixClient {
    type Item = Result<SyncResponse, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        task_trace!(self.ctx, "Polled matrix client");
        self.poll_sync(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::MatrixClient;
    use mockito::{mock, Matcher, Matcher::UrlEncoded};

    #[tokio::test]
    async fn matrix_login() {
        let base_url = mockito::server_url().as_str().parse::<url::Url>().unwrap();
        let username = "sample_username".to_string();
        let password = "sample_password".to_string();

        let mock_req = mock("POST", "/_matrix/client/r0/login")
            .with_header("content-type", "application/json")
            .with_status(200)
            .create();

        let ctx = crate::ConnectionContext::testing_context();

        // run the future to completion. The future will error since invalid json is
        // returned, but as long as the call is correct the error is outside the scope of this
        // test. It is explicitly handled here in case the mock assert panics.
        if let Err(e) = MatrixClient::login(base_url, username, password, ctx).await {
            println! {"MatrixSyncClient returned an error: {:?}", e}
        }

        // give the executor some time to execute the http request on a thread pool
        std::thread::sleep(std::time::Duration::from_millis(200));

        mock_req.assert();
    }

    #[tokio::test]
    async fn send_text_message() {
        let base_url = mockito::server_url().as_str().parse::<url::Url>().unwrap();
        let user_id = "sample_user_id".to_string();
        let token = "sample_token".to_string();
        let room_id = "room_id";

        let mock_req = mock(
            "PUT",
            Matcher::Regex(
                r"^/_matrix/client/r0/rooms/(.+)/send/m.room.message/mircd-(\d+)$".to_string(),
            ),
        )
        .match_query(UrlEncoded("access_token".to_string(), token.clone()))
        .with_status(200)
        .create();

        let httpclient = crate::http::ClientWrapper::new();

        let ctx = crate::ConnectionContext::testing_context();

        let mut client = MatrixClient::new(httpclient, &base_url, user_id, token, ctx);

        // run the future to completion. The future will error since invalid json is
        // returned, but as long as the call is correct the error is outside the scope of this
        // test. It is explicitly handled here in case the mock assert panics.
        if let Err(e) = client
            .send_text_message(room_id, "sample_body".to_string())
            .await
        {
            println! {"MatrixSyncClient returned an error: {:?}", e}
        }

        // give the executor some time to execute the http request on a thread pool
        std::thread::sleep(std::time::Duration::from_millis(200));

        mock_req.assert();
    }
}
