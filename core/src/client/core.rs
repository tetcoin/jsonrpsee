// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::{client::raw::TransportClient, common};
use fnv::FnvHashMap;
use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    error, fmt,
    future::Future,
};

/// Wraps around a [`TransportClient`](crate::TransportClient) and analyzes everything correctly.
///
/// See [the module root documentation](crate::client) for more information.
pub struct Client<R> {
    /// Inner raw client.
    inner: R,

    /// Id to assign to the next request. We always assign linearly-increasing numeric keys.
    next_request_id: ClientRequestId,

    /// List of requests and subscription requests that have been sent out and that are waiting
    /// for a response.
    requests: FnvHashMap<ClientRequestId, Request>,

    /// List of active subscriptions by ID (ID is chosen by the server). Note that this doesn't
    /// cover subscription requests that have been sent out but not answered yet, as these are in
    /// the [`requests`](Client::requests) field.
    ///
    /// The value of this hash map is only ever used for external API purposes and not for
    /// communication with the server.
    ///
    /// Since the keys are decided by the server, we use a regular HashMap and its
    /// hash-collision-resistant algorithm.
    subscriptions: HashMap<String, ClientRequestId>,

    /// Queue of pending events to return from [`Client::next_event`].
    // TODO: call shrink_to from time to time; see https://github.com/rust-lang/rust/issues/56431
    events_queue: VecDeque<ClientEvent>,

    /// Maximum allowed size of [`Client::events_queue`].
    ///
    /// If this size is reached, elements can still be pushed to the queue if they are critical,
    /// but will be discarded if they are not.
    // TODO: make this configurable? note: if this is configurable, it should always be >= 1
    events_queue_max_size: usize,
}

/// Type of request that has been sent out and that is waiting for a response.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// A single request expecting a response.
    Request,
    /// A potential subscription. As a response, we expect a single subscription id.
    PendingSubscription,
    /// The request is stale and was originally used to open a subscription. The subscription ID
    /// decided by the server is contained as parameter.
    ActiveSubscription {
        sub_id: String,
        /// We sent a subscription closing message to the server.
        closing: bool,
    },
    /// Unsubscribing from an active subscription. The request corresponding to the active
    /// subscription to unsubscribe from is contained as parameter.
    Unsubscribe(ClientRequestId),
}

/// Unique identifier of a request within a [`Client`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ClientRequestId(u64);

/// Event returned by [`Client::next_event`].
#[derive(Debug)]
pub enum ClientEvent {
    /// A request has received a response.
    Response {
        /// Identifier of the request. Can be matched with the value that [`Client::start_request`]
        /// has returned.
        request_id: ClientRequestId,
        /// The response itself.
        result: Result<common::JsonValue, common::Error>,
    },

    /// A subscription request has received a response.
    SubscriptionResponse {
        /// Identifier of the request. Can be matched with the value that
        /// [`Client::start_subscription`] has returned.
        request_id: ClientRequestId,
        /// On success, we are now actively subscribed.
        /// [`SubscriptionNotif`](ClientEvent::SubscriptionNotif) events will now be generated.
        result: Result<(), common::Error>,
    },

    /// Notification about something we are subscribed to.
    SubscriptionNotif {
        /// Identifier of the request. Can be matched with the value that
        /// [`Client::start_subscription`] has returned.
        request_id: ClientRequestId,
        /// Opaque data that the server wants to communicate to us.
        result: common::JsonValue,
    },

    /// Finished closing a subscription.
    Unsubscribed {
        /// Subscription that has been closed.
        request_id: ClientRequestId,
    },
}

/// Access to a subscription within a [`Client`].
#[derive(Debug)]
pub enum ClientSubscription<'a, R> {
    /// The server hasn't accepted our subscription request yet.
    Pending(ClientPendingSubscription<'a, R>),
    /// The server has accepted our subscription request. We might receive notifications for it.
    Active(ClientActiveSubscription<'a, R>),
}

/// Access to a subscription within a [`Client`].
#[derive(Debug)]
pub struct ClientPendingSubscription<'a, R> {
    /// Reference to the [`Client`].
    client: &'a mut Client<R>,
    /// Identifier of the subscription within the [`Client`].
    id: ClientRequestId,
}

/// Access to a subscription within a [`Client`].
#[derive(Debug)]
pub struct ClientActiveSubscription<'a, R> {
    /// Reference to the [`Client`].
    client: &'a mut Client<R>,
    /// Identifier of the subscription within the [`Client`].
    id: ClientRequestId,
}

/// Error that can happen during a request.
#[derive(Debug)]
pub enum ClientError<E> {
    /// Error in the raw client.
    Inner(E),
    /// Server returned an error for our request.
    RequestError(common::Error),
    /// Server has sent back a subscription ID that has already been used by an earlier
    /// subscription.
    DuplicateSubscriptionId,
    /// Failed to parse subscription ID send by server.
    ///
    /// On a successful subscription, the server is expected to send back a single number or
    /// string representing the ID of the subscription. This error happens if the server returns
    /// something else than a number or string.
    SubscriptionIdParseError,
    /// Server has sent back a response containing an unknown request ID.
    UnknownRequestId,
    /// Server has sent back a response containing a null request ID.
    NullRequestId,
    /// Server has sent back a notification using an unknown subscription ID.
    UnknownSubscriptionId,
}

/// Error that can happen when attempting to close a subscription.
#[derive(Debug)]
pub enum CloseError<E> {
    /// Error in the raw client.
    TransportClient(E),

    /// We are already trying to close this subscription.
    AlreadyClosing,
}

impl<R> Client<R> {
    /// Initializes a new `Client` using the given raw client as backend.
    pub fn new(inner: R) -> Self {
        Client {
            inner,
            next_request_id: ClientRequestId(0),
            requests: FnvHashMap::default(),
            subscriptions: HashMap::default(),
            events_queue: VecDeque::with_capacity(16),
            events_queue_max_size: 64,
        }
    }
}

impl<R> Client<R>
where
    R: TransportClient,
{
    /// Sends a notification to the server. The notification doesn't need any response.
    ///
    /// This asynchronous function finishes when the notification has finished being sent.
    pub async fn send_notification(
        &mut self,
        method: impl Into<String>,
        params: impl Into<common::Params>,
    ) -> Result<(), R::Error> {
        let request = common::Request::Single(common::Call::Notification(common::Notification {
            jsonrpc: common::Version::V2,
            method: method.into(),
            params: params.into(),
        }));

        self.inner.send_request(request).await?;
        Ok(())
    }

    /// Starts a request.
    ///
    /// This asynchronous function finishes when the request has been sent to the server. The
    /// request is added to the [`Client`]. You must then call [`next_event`](Client::next_event)
    /// until you get a response.
    pub async fn start_request(
        &mut self,
        method: impl Into<String>,
        params: impl Into<common::Params>,
    ) -> Result<ClientRequestId, R::Error> {
        self.start_impl(method, params, Request::Request).await
    }

    /// Starts a request.
    ///
    /// This asynchronous function finishes when the request has been sent to the server. The
    /// request is added to the [`Client`]. You must then call [`next_event`](Client::next_event)
    /// until you get a response.
    pub async fn start_subscription(
        &mut self,
        method: impl Into<String>,
        params: impl Into<common::Params>,
    ) -> Result<ClientRequestId, R::Error> {
        self.start_impl(method, params, Request::PendingSubscription)
            .await
    }

    /// Inner implementation for starting either a request or a subscription.
    async fn start_impl(
        &mut self,
        method: impl Into<String>,
        params: impl Into<common::Params>,
        ty: Request,
    ) -> Result<ClientRequestId, R::Error> {
        loop {
            let id = self.next_request_id;
            self.next_request_id.0 = self.next_request_id.0.wrapping_add(1);

            let entry = match self.requests.entry(id) {
                Entry::Occupied(_) => continue,
                Entry::Vacant(e) => e,
            };

            let request = common::Request::Single(common::Call::MethodCall(common::MethodCall {
                jsonrpc: common::Version::V2,
                method: method.into(),
                params: params.into(),
                id: common::Id::Num(id.0),
            }));

            // Note that in case of an error, we "lose" the request id (as in, it will never be
            // used). This isn't a problem, however.
            self.inner.send_request(request).await?;

            entry.insert(ty);
            break Ok(id);
        }
    }

    /// Waits until the client receives a message from the server.
    ///
    /// If this function returns an `Err`, it indicates a connectivity issue with the server or a
    /// low-level protocol error, and not a request that has failed to be answered.
    pub async fn next_event(&mut self) -> Result<ClientEvent, ClientError<R::Error>> {
        loop {
            if let Some(event) = self.events_queue.pop_front() {
                return Ok(event);
            }

            self.event_step().await?;
        }
    }

    /// Returns a `Future` that resolves when the server sends back a response for the given
    /// request.
    ///
    /// Returns `None` if the request identifier is invalid, or if the request is a subscription.
    ///
    /// > **Note**: While this function is waiting, all the other responses and pubsub events
    /// >           returned by the server will be buffered up to a certain limit. Once this
    /// >           limit is reached, server notifications will be discarded. If you want to be
    /// >           sure to catch all notifications, use [`next_event`](Client::next_event)
    /// >           instead.
    pub fn request_by_id<'a>(
        &'a mut self,
        rq_id: ClientRequestId,
    ) -> Option<impl Future<Output = Result<common::JsonValue, ClientError<R::Error>>> + 'a> {
        // First, let's check whether the request ID is valid.
        if let Some(rq) = self.requests.get(&rq_id) {
            if *rq != Request::Request {
                return None;
            }
        } else {
            return None;
        }

        Some(async move {
            let mut events_queue_loopkup = 0;

            loop {
                while events_queue_loopkup < self.events_queue.len() {
                    match &self.events_queue[events_queue_loopkup] {
                        ClientEvent::Response { request_id, .. } if *request_id == rq_id => {
                            return match self.events_queue.remove(events_queue_loopkup) {
                                Some(ClientEvent::Response { result, .. }) => {
                                    result.map_err(ClientError::RequestError)
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => {}
                    }

                    events_queue_loopkup += 1;
                }

                self.event_step().await?;
            }
        })
    }

    /// Returns a [`ClientSubscription`] object representing a certain active or pending
    /// subscription.
    ///
    /// Returns `None` if the identifier is invalid, or if it is not a subscription.
    pub fn subscription_by_id(&mut self, rq_id: ClientRequestId) -> Option<ClientSubscription<R>> {
        match self.requests.get(&rq_id)? {
            Request::PendingSubscription => {
                debug_assert!(!self.subscriptions.values().any(|i| *i == rq_id));
                Some(ClientSubscription::Pending(ClientPendingSubscription {
                    client: self,
                    id: rq_id,
                }))
            }

            Request::ActiveSubscription { sub_id, .. } => {
                debug_assert_eq!(self.subscriptions.get(sub_id), Some(&rq_id));
                Some(ClientSubscription::Active(ClientActiveSubscription {
                    client: self,
                    id: rq_id,
                }))
            }

            _ => None,
        }
    }

    /// Waits for one server message and processes it by updating the state of `self`.
    ///
    /// If the events queue is full (see [`Client::events_queue_max_size`]), then responses to
    /// requests will still be pushed to the queue, but notifications will be discarded.
    ///
    /// Check the content of [`events_queue`](Client::events_queue) afterwards for events to
    /// dispatch to the user.
    async fn event_step(&mut self) -> Result<(), ClientError<R::Error>> {
        let result = self
            .inner
            .next_response()
            .await
            .map_err(ClientError::Inner)?;

        match result {
            common::Response::Single(rp) => self.process_response(rp)?,
            common::Response::Batch(rps) => {
                for rp in rps {
                    // TODO: if an errror happens, we throw away the entire batch
                    self.process_response(rp)?;
                }
            }
            common::Response::Notif(notif) => {
                let sub_id = notif.params.subscription.into_string();
                if let Some(request_id) = self.subscriptions.get(&sub_id) {
                    if self.events_queue.len() < self.events_queue_max_size {
                        self.events_queue.push_back(ClientEvent::SubscriptionNotif {
                            request_id: *request_id,
                            result: notif.params.result,
                        });
                    }
                } else {
                    log::warn!(
                        "Server sent subscription notif with an invalid id: {:?}",
                        sub_id
                    );
                    return Err(ClientError::UnknownSubscriptionId);
                }
            }
        }

        Ok(())
    }

    /// Processes the response obtained from the server. Updates the internal state of `self` to
    /// account for it.
    fn process_response(&mut self, response: common::Output) -> Result<(), ClientError<R::Error>> {
        let request_id = match response.id() {
            common::Id::Num(n) => ClientRequestId(*n),
            common::Id::Str(s) => {
                log::warn!("Server responded with an invalid request id: {:?}", s);
                return Err(ClientError::UnknownRequestId);
            }
            common::Id::Null => {
                log::warn!("Server responded with a null request id");
                return Err(ClientError::NullRequestId);
            }
        };

        // Find the request that this answered.
        match self.requests.remove(&request_id) {
            Some(Request::Request) => {
                self.events_queue.push_back(ClientEvent::Response {
                    result: response.into(),
                    request_id,
                });
            }

            Some(Request::PendingSubscription) => {
                let response = match Result::from(response) {
                    Ok(r) => r,
                    Err(err) => {
                        self.events_queue
                            .push_back(ClientEvent::SubscriptionResponse {
                                result: Err(err),
                                request_id,
                            });
                        return Ok(());
                    }
                };

                let sub_id = match common::from_value::<common::SubscriptionId>(response) {
                    Ok(id) => id.into_string(),
                    Err(err) => {
                        log::warn!("Failed to parse string subscription id: {:?}", err);
                        return Err(ClientError::SubscriptionIdParseError);
                    }
                };

                match self.subscriptions.entry(sub_id.clone()) {
                    Entry::Vacant(e) => e.insert(request_id),
                    Entry::Occupied(e) => {
                        log::warn!("Duplicate subscription id sent by server: {:?}", e.key());
                        return Err(ClientError::DuplicateSubscriptionId);
                    }
                };

                self.requests.insert(
                    request_id,
                    Request::ActiveSubscription {
                        sub_id,
                        closing: false,
                    },
                );
                self.events_queue
                    .push_back(ClientEvent::SubscriptionResponse {
                        result: Ok(()),
                        request_id,
                    });
            }

            Some(Request::Unsubscribe(active_sub_rq_id)) => {
                match self.requests.remove(&active_sub_rq_id) {
                    Some(Request::ActiveSubscription { sub_id, .. }) => {
                        if self.subscriptions.remove(&sub_id).is_some() {
                            self.events_queue.push_back(ClientEvent::Unsubscribed {
                                request_id: active_sub_rq_id,
                            });
                        } else {
                            debug_assert!(false);
                        }
                    }
                    _ => debug_assert!(false),
                }
            }

            Some(v @ Request::ActiveSubscription { .. }) => {
                self.requests.insert(request_id, v);
                log::warn!(
                    "Server responsed with an invalid request id: {:?}",
                    request_id
                );
                return Err(ClientError::UnknownRequestId);
            }

            None => {
                log::warn!(
                    "Server responsed with an invalid request id: {:?}",
                    request_id
                );
                return Err(ClientError::UnknownRequestId);
            }
        };

        Ok(())
    }
}

impl<R> fmt::Debug for Client<R>
where
    R: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Client")
            .field("inner", &self.inner)
            .field("pending_requests", &self.requests.keys())
            .field("active_subscriptions", &self.subscriptions.keys())
            .finish()
    }
}

impl<'a, R> ClientSubscription<'a, R>
where
    R: TransportClient,
{
    /// Returns true if the subscription is active. That is, if the server has accepted our
    /// subscription request and might generate events.
    pub fn is_active(&self) -> bool {
        match self {
            ClientSubscription::Pending(_) => false,
            ClientSubscription::Active(_) => true,
        }
    }
}

impl<'a, R> ClientPendingSubscription<'a, R>
where
    R: TransportClient,
{
    // TODO: since this is the only method, maybe we could replace `ClientPendingSubscription`
    //       with an `impl Future` once the `impl Trait` feature is stabilized
    /// Wait until the server sends back an answer to this subscription request.
    ///
    /// > **Note**: While this function is waiting, all the other responses and pubsub events
    /// >           returned by the server will be buffered up to a certain limit. Once this
    /// >           limit is reached, server notifications will be discarded. If you want to be
    /// >           sure to catch all notifications, use [`next_event`](Client::next_event)
    /// >           instead.
    pub async fn wait(self) -> Result<ClientActiveSubscription<'a, R>, ClientError<R::Error>> {
        let mut events_queue_loopkup = 0;

        loop {
            while events_queue_loopkup < self.client.events_queue.len() {
                match &self.client.events_queue[events_queue_loopkup] {
                    ClientEvent::SubscriptionResponse { request_id, .. }
                        if *request_id == self.id =>
                    {
                        return match self.client.events_queue.remove(events_queue_loopkup) {
                            Some(ClientEvent::SubscriptionResponse { result: Ok(()), .. }) => {
                                Ok(ClientActiveSubscription {
                                    client: self.client,
                                    id: self.id,
                                })
                            }
                            Some(ClientEvent::SubscriptionResponse {
                                result: Err(err), ..
                            }) => Err(ClientError::RequestError(err)),
                            _ => unreachable!(),
                        }
                    }
                    _ => {}
                }

                events_queue_loopkup += 1;
            }

            self.client.event_step().await?;
        }
    }
}

impl<'a, R> ClientActiveSubscription<'a, R>
where
    R: TransportClient,
{
    /// Returns a `Future` that resolves when the server sends back a notification for this
    /// subscription.
    ///
    /// > **Note**: While this function is waiting, all the other responses and pubsub events
    /// >           returned by the server will be buffered up to a certain limit. Once this
    /// >           limit is reached, server notifications will be discarded. If you want to be
    /// >           sure to catch all notifications, use [`next_event`](Client::next_event)
    /// >           instead.
    pub async fn next_notification(&mut self) -> Result<common::JsonValue, ClientError<R::Error>> {
        let mut events_queue_loopkup = 0;

        loop {
            while events_queue_loopkup < self.client.events_queue.len() {
                match &self.client.events_queue[events_queue_loopkup] {
                    ClientEvent::SubscriptionNotif { request_id, .. } if *request_id == self.id => {
                        return match self.client.events_queue.remove(events_queue_loopkup) {
                            Some(ClientEvent::SubscriptionNotif { result, .. }) => Ok(result),
                            _ => unreachable!(),
                        }
                    }
                    _ => {}
                }

                events_queue_loopkup += 1;
            }

            self.client.event_step().await?;
        }
    }

    /// Returns `true` if we called [`close`](ClientActiveSubscription::close) earlier on this
    /// subscription and we are waiting for the server to respond to our close request.
    pub fn is_closing(&self) -> bool {
        match self.client.requests.get(&self.id) {
            Some(Request::ActiveSubscription { closing, .. }) => *closing,
            _ => panic!(),
        }
    }

    /// Starts closing an open subscription by performing an RPC call with the given method name.
    ///
    /// Calling this method multiple times with the same subscription will yield an error.
    ///
    /// Note that, for convenience, we will consider the subscription closed even the server
    /// returns an error to the unsubscription request.
    pub async fn close(
        &mut self,
        method_name: impl Into<String>,
    ) -> Result<(), CloseError<R::Error>> {
        let sub_id = match self.client.requests.get(&self.id) {
            Some(Request::ActiveSubscription { sub_id, closing }) => {
                if *closing {
                    return Err(CloseError::AlreadyClosing);
                }
                sub_id.clone()
            }
            _ => panic!(),
        };

        let params = common::Params::Array(vec![sub_id.clone().into()]);
        self.client
            .start_impl(method_name, params, Request::Unsubscribe(self.id))
            .await
            .map_err(CloseError::TransportClient)?;

        match self.client.requests.get_mut(&self.id) {
            Some(Request::ActiveSubscription { closing, .. }) => {
                debug_assert!(!*closing);
                *closing = true;
            }
            _ => panic!(),
        };

        Ok(())
    }
}

impl<E> error::Error for ClientError<E>
where
    E: error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            ClientError::Inner(ref err) => Some(err),
            ClientError::RequestError(ref err) => Some(err),
            ClientError::DuplicateSubscriptionId => None,
            ClientError::SubscriptionIdParseError => None,
            ClientError::UnknownRequestId => None,
            ClientError::NullRequestId => None,
            ClientError::UnknownSubscriptionId => None,
        }
    }
}

impl<E> fmt::Display for ClientError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ClientError::Inner(ref err) => write!(f, "Error in the raw client: {}", err),
            ClientError::RequestError(ref err) => write!(f, "Server returned error: {}", err),
            ClientError::DuplicateSubscriptionId => write!(
                f,
                "Server has responded with a subscription ID that's already in use"
            ),
            ClientError::SubscriptionIdParseError => write!(f, "Subscription ID parse error"),
            ClientError::UnknownRequestId => {
                write!(f, "Server responded with an unknown request ID")
            }
            ClientError::NullRequestId => write!(f, "Server responded with a null request ID"),
            ClientError::UnknownSubscriptionId => {
                write!(f, "Server responded with an unknown subscription ID")
            }
        }
    }
}

impl<E> error::Error for CloseError<E>
where
    E: error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            CloseError::TransportClient(err) => Some(err),
            CloseError::AlreadyClosing => None,
        }
    }
}

impl<E> fmt::Display for CloseError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CloseError::TransportClient(err) => fmt::Display::fmt(err, f),
            CloseError::AlreadyClosing => write!(f, "Subscription already being closed"),
        }
    }
}
