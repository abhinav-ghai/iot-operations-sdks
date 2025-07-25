// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client for State Store operations.
//!
//! To use this client, the `state_store` feature must be enabled.

use std::{collections::HashMap, sync::Arc, time::Duration};

use azure_iot_operations_mqtt::{
    interface::{AckToken, ManagedClient},
    session::SessionConnectionMonitor,
};
use azure_iot_operations_protocol::{
    application::ApplicationContext, common::hybrid_logical_clock::HybridLogicalClock, rpc_command,
    telemetry,
};
use data_encoding::HEXUPPER;
use derive_builder::Builder;
use tokio::{sync::Notify, task};

use crate::common::dispatcher::{DispatchError, DispatchErrorKind, Dispatcher, Receiver};
use crate::state_store::{self, Error, ErrorKind, FENCING_TOKEN_USER_PROPERTY, SetOptions};

const REQUEST_TOPIC_PATTERN: &str =
    "statestore/v1/FA9AE35F-2F64-47CD-9BFF-08E2B32A0FE8/command/invoke";
const RESPONSE_TOPIC_PREFIX: &str = "clients/{invokerClientId}/services";
const RESPONSE_TOPIC_SUFFIX: &str = "response";
const COMMAND_NAME: &str = "invoke";
// where the encodedClientId is an upper-case hex encoded representation of the MQTT ClientId of the client that initiated the KEYNOTIFY request and encodedKeyName is a hex encoded representation of the key that changed
const NOTIFICATION_TOPIC_PATTERN: &str = "clients/statestore/v1/FA9AE35F-2F64-47CD-9BFF-08E2B32A0FE8/{encodedClientId}/command/notify/{encodedKeyName}";

/// A struct to manage receiving notifications for a key
#[derive(Debug)]
pub struct KeyObservation {
    /// The name of the key (for convenience)
    pub key: Vec<u8>,
    /// The internal channel for receiving notifications for this key
    receiver: Receiver<(state_store::KeyNotification, Option<AckToken>)>,
}
impl KeyObservation {
    /// Receives a [`state_store::KeyNotification`] or [`None`] if there will be no more notifications.
    ///
    /// If there are notifications:
    /// - Returns Some([`state_store::KeyNotification`], [`Option<AckToken>`]) on success
    ///     - If auto ack is disabled, the [`AckToken`] should be used or dropped when you want the ack to occur. If auto ack is enabled, you may use ([`state_store::KeyNotification`], _) to ignore the [`AckToken`].
    ///
    /// A received notification can be acknowledged via the [`AckToken`] by calling [`AckToken::ack`] or dropping the [`AckToken`].
    pub async fn recv_notification(
        &mut self,
    ) -> Option<(state_store::KeyNotification, Option<AckToken>)> {
        self.receiver.recv().await
    }

    // on drop, don't remove from hashmap so we can differentiate between a key
    // that was observed where the receiver was dropped and a key that was never observed
}

/// State Store Client Options struct
#[derive(Builder, Clone)]
#[builder(setter(into))]
pub struct ClientOptions {
    /// If true, key notifications are auto-acknowledged
    #[builder(default = "true")]
    key_notification_auto_ack: bool,
}

/// State store client implementation
pub struct Client<C>
where
    C: ManagedClient + Clone + Send + Sync + 'static,
    C::PubReceiver: Send + Sync,
{
    invoker: rpc_command::Invoker<state_store::resp3::Request, state_store::resp3::Response, C>,
    notification_dispatcher:
        Arc<Dispatcher<(state_store::KeyNotification, Option<AckToken>), String>>,
    shutdown_notifier: Arc<Notify>,
}

impl<C> Client<C>
where
    C: ManagedClient + Clone + Send + Sync,
    C::PubReceiver: Send + Sync,
{
    /// Create a new State Store Client
    ///
    /// <div class="warning">
    ///
    /// Note: `connection_monitor` must be from the same session as `client`.
    ///
    /// </div>
    ///
    /// # Errors
    /// [`struct@Error`] of kind [`AIOProtocolError`](ErrorKind::AIOProtocolError) is possible if
    ///     there are any errors creating the underlying command invoker or telemetry receiver, but it should not happen
    ///
    /// # Panics
    /// Possible panics when building options for the underlying command invoker or telemetry receiver,
    /// but they should be unreachable because we control the static parameters that go into these calls.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(
        application_context: ApplicationContext,
        client: C,
        connection_monitor: SessionConnectionMonitor,
        options: ClientOptions,
    ) -> Result<Self, Error> {
        // create invoker for commands
        let invoker_options = rpc_command::invoker::OptionsBuilder::default()
            .request_topic_pattern(REQUEST_TOPIC_PATTERN)
            .response_topic_prefix(Some(RESPONSE_TOPIC_PREFIX.into()))
            .response_topic_suffix(Some(RESPONSE_TOPIC_SUFFIX.into()))
            .topic_token_map(HashMap::from([("invokerClientId".to_string(), client.client_id().to_string())]))
            .command_name(COMMAND_NAME)
            .build()
            .expect("Unreachable because all parameters that could cause errors are statically provided");

        let invoker: rpc_command::Invoker<
            state_store::resp3::Request,
            state_store::resp3::Response,
            C,
        > = rpc_command::Invoker::new(application_context.clone(), client.clone(), invoker_options)
            .map_err(ErrorKind::from)?;

        // Create the uppercase hex encoded version of the client ID that is used in the key notification topic
        let encoded_client_id = HEXUPPER.encode(client.client_id().as_bytes());

        // create telemetry receiver for notifications
        let receiver_options = telemetry::receiver::OptionsBuilder::default()
            .topic_pattern(NOTIFICATION_TOPIC_PATTERN)
            .topic_token_map(HashMap::from([(
                "encodedClientId".to_string(),
                encoded_client_id),
                ]))
            .auto_ack(options.key_notification_auto_ack)
            .build()
            .expect("Unreachable because all parameters that could cause errors are statically provided");

        // Create the shutdown notifier for the receiver loop
        let shutdown_notifier = Arc::new(Notify::new());

        // Create a hashmap of keys being observed and channels to send their notifications to
        let notification_dispatcher = Arc::new(Dispatcher::new());

        // Start the receive key notification loop
        task::spawn({
            let notification_receiver: telemetry::Receiver<state_store::resp3::Operation, C> =
                telemetry::Receiver::new(application_context, client, receiver_options)
                    .map_err(ErrorKind::from)?;
            let shutdown_notifier_clone = shutdown_notifier.clone();
            let notification_dispatcher_clone = notification_dispatcher.clone();
            async move {
                Self::receive_key_notification_loop(
                    shutdown_notifier_clone,
                    notification_receiver,
                    notification_dispatcher_clone,
                    connection_monitor,
                )
                .await;
            }
        });

        Ok(Self {
            invoker,
            notification_dispatcher,
            shutdown_notifier,
        })
    }

    /// Shutdown the [`state_store::Client`]. Shuts down the command invoker and telemetry receiver
    /// and cancels the receiver loop to drop the receiver and to prevent the task from looping indefinitely.
    ///
    /// Note: If this method is called, the [`state_store::Client`] should not be used again.
    /// If the method returns an error, it may be called again to attempt the unsubscribe again.
    ///
    /// Returns Ok(()) on success, otherwise returns [`struct@Error`].
    /// # Errors
    /// [`struct@Error`] of kind [`AIOProtocolError`](ErrorKind::AIOProtocolError) if the unsubscribe fails or if the unsuback reason code doesn't indicate success.
    pub async fn shutdown(&self) -> Result<(), Error> {
        // Notify the receiver loop to shutdown the telemetry receiver
        self.shutdown_notifier.notify_one();

        self.invoker.shutdown().await.map_err(ErrorKind::from)?;

        log::info!("Shutdown");
        Ok(())
    }

    /// Sets a key value pair in the State Store Service
    ///
    /// Note: timeout refers to the duration until the State Store Client stops
    /// waiting for a `Set` response from the Service. This value is not linked
    /// to the key in the State Store. It is rounded up to the nearest second.
    ///
    /// Returns `true` if the `Set` completed successfully, or `false` if the `Set` did not occur because of values specified in `SetOptions`
    /// # Errors
    /// [`struct@Error`] of kind [`InvalidArgument`](ErrorKind::InvalidArgument) if:
    /// - the `key` is empty
    /// - the `timeout` is zero or > `u32::max`
    ///
    /// [`struct@Error`] of kind [`ServiceError`](ErrorKind::ServiceError) if the State Store returns an Error response
    ///
    /// [`struct@Error`] of kind [`UnexpectedPayload`](ErrorKind::UnexpectedPayload) if the State Store returns a response that isn't valid for a `Set` request
    ///
    /// [`struct@Error`] of kind [`AIOProtocolError`](ErrorKind::AIOProtocolError) if there are any underlying errors from [`rpc_command::Invoker::invoke`]
    pub async fn set(
        &self,
        key: Vec<u8>,
        value: Vec<u8>,
        timeout: Duration,
        fencing_token: Option<HybridLogicalClock>,
        options: SetOptions,
    ) -> Result<state_store::Response<bool>, Error> {
        if key.is_empty() {
            return Err(Error(ErrorKind::InvalidArgument(
                "key is empty".to_string(),
            )));
        }
        let mut request_builder = rpc_command::invoker::RequestBuilder::default();
        request_builder
            .payload(state_store::resp3::Request::Set {
                key,
                value,
                options: options.clone(),
            })
            .map_err(|e| ErrorKind::SerializationError(e.to_string()))? // this can't fail
            .timeout(timeout);
        if let Some(ft) = fencing_token {
            request_builder.custom_user_data(vec![(
                FENCING_TOKEN_USER_PROPERTY.to_string(),
                ft.to_string(),
            )]);
        }
        let request = request_builder
            .build()
            .map_err(|e| ErrorKind::InvalidArgument(e.to_string()))?;
        state_store::convert_response(
            self.invoker
                .invoke(request)
                .await
                .map_err(ErrorKind::from)?,
            |payload| match payload {
                state_store::resp3::Response::NotApplied => Ok(false),
                state_store::resp3::Response::Ok => Ok(true),
                _ => Err(()),
            },
        )
    }

    /// Gets the value of a key in the State Store Service
    ///
    /// Note: timeout refers to the duration until the State Store Client stops
    /// waiting for a `Get` response from the Service. This value is not linked
    /// to the key in the State Store. It is rounded up to the nearest second.
    ///
    /// Returns `Some(<value of the key>)` if the key is found or `None` if the key was not found
    /// # Errors
    /// [`struct@Error`] of kind [`InvalidArgument`](ErrorKind::InvalidArgument) if:
    /// - the `key` is empty
    /// - the `timeout` is zero or > `u32::max`
    ///
    /// [`struct@Error`] of kind [`ServiceError`](ErrorKind::ServiceError) if the State Store returns an Error response
    ///
    /// [`struct@Error`] of kind [`UnexpectedPayload`](ErrorKind::UnexpectedPayload) if the State Store returns a response that isn't valid for a `Get` request
    ///
    /// [`struct@Error`] of kind [`AIOProtocolError`](ErrorKind::AIOProtocolError) if there are any underlying errors from [`rpc_command::Invoker::invoke`]
    pub async fn get(
        &self,
        key: Vec<u8>,
        timeout: Duration,
    ) -> Result<state_store::Response<Option<Vec<u8>>>, Error> {
        if key.is_empty() {
            return Err(Error(ErrorKind::InvalidArgument(
                "key is empty".to_string(),
            )));
        }
        let request = rpc_command::invoker::RequestBuilder::default()
            .payload(state_store::resp3::Request::Get { key })
            .map_err(|e| ErrorKind::SerializationError(e.to_string()))? // this can't fail
            .timeout(timeout)
            .build()
            .map_err(|e| ErrorKind::InvalidArgument(e.to_string()))?;
        state_store::convert_response(
            self.invoker
                .invoke(request)
                .await
                .map_err(ErrorKind::from)?,
            |payload| match payload {
                state_store::resp3::Response::Value(value) => Ok(Some(value)),
                state_store::resp3::Response::NotFound => Ok(None),
                _ => Err(()),
            },
        )
    }

    /// Deletes a key from the State Store Service
    ///
    /// Note: timeout refers to the duration until the State Store Client stops
    /// waiting for a `Delete` response from the Service. This value is not linked
    /// to the key in the State Store. It is rounded up to the nearest second.
    ///
    /// Returns the number of keys deleted. Will be `0` if the key was not found, otherwise `1`
    /// # Errors
    /// [`struct@Error`] of kind [`InvalidArgument`](ErrorKind::InvalidArgument) if:
    /// - the `key` is empty
    /// - the `timeout` is zero or > `u32::max`
    ///
    /// [`struct@Error`] of kind [`ServiceError`](ErrorKind::ServiceError) if the State Store returns an Error response
    ///
    /// [`struct@Error`] of kind [`UnexpectedPayload`](ErrorKind::UnexpectedPayload) if the State Store returns a response that isn't valid for a `Delete` request
    ///
    /// [`struct@Error`] of kind [`AIOProtocolError`](ErrorKind::AIOProtocolError) if there are any underlying errors from [`rpc_command::Invoker::invoke`]
    pub async fn del(
        &self,
        key: Vec<u8>,
        fencing_token: Option<HybridLogicalClock>,
        timeout: Duration,
    ) -> Result<state_store::Response<i64>, Error> {
        if key.is_empty() {
            return Err(Error(ErrorKind::InvalidArgument(
                "key is empty".to_string(),
            )));
        }
        self.del_internal(
            state_store::resp3::Request::Del { key },
            fencing_token,
            timeout,
        )
        .await
    }

    /// Deletes a key from the State Store Service if and only if the value matches the one provided
    ///
    /// Note: timeout refers to the duration until the State Store Client stops
    /// waiting for a `V Delete` response from the Service. This value is not linked
    /// to the key in the State Store. It is rounded up to the nearest second.
    ///
    /// Returns the number of keys deleted. Will be `0` if the key was not found, `-1` if the value did not match, otherwise `1`
    /// # Errors
    /// [`struct@Error`] of kind [`InvalidArgument`](ErrorKind::InvalidArgument) if:
    /// - the `key` is empty
    /// - the `timeout` is zero or > `u32::max`
    ///
    /// [`struct@Error`] of kind [`ServiceError`](ErrorKind::ServiceError) if the State Store returns an Error response
    ///
    /// [`struct@Error`] of kind [`UnexpectedPayload`](ErrorKind::UnexpectedPayload) if the State Store returns a response that isn't valid for a `V Delete` request
    ///
    /// [`struct@Error`] of kind [`AIOProtocolError`](ErrorKind::AIOProtocolError) if there are any underlying errors from [`rpc_command::Invoker::invoke`]
    pub async fn vdel(
        &self,
        key: Vec<u8>,
        value: Vec<u8>,
        fencing_token: Option<HybridLogicalClock>,
        timeout: Duration,
    ) -> Result<state_store::Response<i64>, Error> {
        if key.is_empty() {
            return Err(Error(ErrorKind::InvalidArgument(
                "key is empty".to_string(),
            )));
        }
        self.del_internal(
            state_store::resp3::Request::VDel { key, value },
            fencing_token,
            timeout,
        )
        .await
    }

    async fn del_internal(
        &self,
        request: state_store::resp3::Request,
        fencing_token: Option<HybridLogicalClock>,
        timeout: Duration,
    ) -> Result<state_store::Response<i64>, Error> {
        let mut request_builder = rpc_command::invoker::RequestBuilder::default();
        request_builder
            .payload(request)
            .map_err(|e| ErrorKind::SerializationError(e.to_string()))? // this can't fail
            .timeout(timeout);
        if let Some(ft) = fencing_token {
            request_builder.custom_user_data(vec![(
                FENCING_TOKEN_USER_PROPERTY.to_string(),
                ft.to_string(),
            )]);
        }
        let request = request_builder
            .build()
            .map_err(|e| ErrorKind::InvalidArgument(e.to_string()))?;
        state_store::convert_response(
            self.invoker
                .invoke(request)
                .await
                .map_err(ErrorKind::from)?,
            |payload| match payload {
                state_store::resp3::Response::NotFound => Ok(0),
                state_store::resp3::Response::NotApplied => Ok(-1),
                state_store::resp3::Response::ValuesDeleted(value) => Ok(value),
                _ => Err(()),
            },
        )
    }

    /// Internal function calling invoke for observe command to allow all errors to be captured in one place
    async fn invoke_observe(
        &self,
        key: Vec<u8>,
        timeout: Duration,
    ) -> Result<state_store::Response<()>, Error> {
        // Send invoke request for observe
        let request = rpc_command::invoker::RequestBuilder::default()
            .payload(state_store::resp3::Request::KeyNotify {
                key: key.clone(),
                options: state_store::resp3::KeyNotifyOptions { stop: false },
            })
            .map_err(|e| ErrorKind::SerializationError(e.to_string()))? // this can't fail
            .timeout(timeout)
            .build()
            .map_err(|e| ErrorKind::InvalidArgument(e.to_string()))?;

        state_store::convert_response(
            self.invoker
                .invoke(request)
                .await
                .map_err(ErrorKind::from)?,
            |payload| match payload {
                state_store::resp3::Response::Ok => Ok(()),
                _ => Err(()),
            },
        )
    }

    /// Starts observation of any changes on a key from the State Store Service
    ///
    /// Note: `timeout` is rounded up to the nearest second.
    ///
    /// Returns OK([`state_store::Response<KeyObservation>`]) if the key is now being observed.
    /// The [`KeyObservation`] can be used to receive key notifications for this key
    ///
    /// <div class="warning">
    ///
    /// If a client disconnects, it must resend the Observe for any keys
    /// it needs to continue monitoring. Unlike MQTT subscriptions, which can be
    /// persisted across a nonclean session, the state store internally removes
    /// any key observations when a given client disconnects. This is a known
    /// limitation of the service, see [here](https://learn.microsoft.com/azure/iot-operations/create-edge-apps/concept-about-state-store-protocol#keynotify-notification-topics-and-lifecycle)
    /// for more information
    ///
    /// </div>
    ///
    /// # Errors
    /// [`struct@Error`] of kind [`InvalidArgument`](ErrorKind::InvalidArgument) if:
    /// - the `key` is empty
    /// - the `timeout` is zero or > `u32::max`
    ///
    /// [`struct@Error`] of kind [`DuplicateObserve`](ErrorKind::DuplicateObserve) if
    /// - the key is already being observed by this client
    ///
    /// [`struct@Error`] of kind [`ServiceError`](ErrorKind::ServiceError) if
    /// - the State Store returns an Error response
    /// - the State Store returns a response that isn't valid for an `Observe` request
    ///
    /// [`struct@Error`] of kind [`AIOProtocolError`](ErrorKind::AIOProtocolError) if
    /// - there are any underlying errors from [`rpc_command::Invoker::invoke`]
    pub async fn observe(
        &self,
        key: Vec<u8>,
        timeout: Duration,
    ) -> Result<state_store::Response<KeyObservation>, Error> {
        if key.is_empty() {
            return Err(Error(ErrorKind::InvalidArgument(
                "key is empty".to_string(),
            )));
        }

        // add to observed keys before sending command to prevent missing any notifications.
        // If the observe request fails, this entry will be removed before the function returns
        let encoded_key_name = HEXUPPER.encode(&key);

        let rx = self
            .notification_dispatcher
            .register_receiver(encoded_key_name.clone())
            .map_err(|_| Error(ErrorKind::DuplicateObserve))?;

        // Capture any errors from the command invoke so we can remove the key from the observed_keys hashmap
        match self.invoke_observe(key.clone(), timeout).await {
            Ok(r) => Ok(state_store::Response {
                response: KeyObservation { key, receiver: rx },
                version: r.version,
            }),
            Err(e) => {
                // if the observe request wasn't successful, remove it from our dispatcher
                if self
                    .notification_dispatcher
                    .unregister_receiver(&encoded_key_name)
                {
                    log::debug!("key removed from observed list: {encoded_key_name:?}");
                } else {
                    log::debug!("key not in observed list: {encoded_key_name:?}");
                }
                Err(e)
            }
        }
    }

    /// Stops observation of any changes on a key from the State Store Service
    ///
    /// Note: `timeout` is rounded up to the nearest second.
    ///
    /// Returns `true` if the key is no longer being observed or `false` if the key wasn't being observed
    /// # Errors
    /// [`struct@Error`] of kind [`InvalidArgument`](ErrorKind::InvalidArgument) if:
    /// - the `key` is empty
    /// - the `timeout` is zero or > `u32::max`
    ///
    /// [`struct@Error`] of kind [`ServiceError`](ErrorKind::ServiceError) if
    /// - the State Store returns an Error response
    /// - the State Store returns a response that isn't valid for an `Unobserve` request
    ///
    /// [`struct@Error`] of kind [`AIOProtocolError`](ErrorKind::AIOProtocolError) if
    /// - there are any underlying errors from [`rpc_command::Invoker::invoke`]
    pub async fn unobserve(
        &self,
        key: Vec<u8>,
        timeout: Duration,
    ) -> Result<state_store::Response<bool>, Error> {
        if key.is_empty() {
            return Err(Error(ErrorKind::InvalidArgument(
                "key is empty".to_string(),
            )));
        }
        // Send invoke request for unobserve
        let request = rpc_command::invoker::RequestBuilder::default()
            .payload(state_store::resp3::Request::KeyNotify {
                key: key.clone(),
                options: state_store::resp3::KeyNotifyOptions { stop: true },
            })
            .map_err(|e| ErrorKind::SerializationError(e.to_string()))? // this can't fail
            .timeout(timeout)
            .build()
            .map_err(|e| ErrorKind::InvalidArgument(e.to_string()))?;
        match state_store::convert_response(
            self.invoker
                .invoke(request)
                .await
                .map_err(ErrorKind::from)?,
            |payload| match payload {
                state_store::resp3::Response::Ok => Ok(true),
                state_store::resp3::Response::NotFound => Ok(false),
                _ => Err(()),
            },
        ) {
            Ok(r) => {
                // remove key from observed_keys hashmap
                let encoded_key_name = HEXUPPER.encode(&key);

                if self
                    .notification_dispatcher
                    .unregister_receiver(&encoded_key_name)
                {
                    log::debug!("key removed from observed list: {encoded_key_name:?}");
                } else {
                    log::debug!("key not in observed list: {encoded_key_name:?}");
                }
                Ok(r)
            }
            Err(e) => Err(e),
        }
    }

    /// only return when the session goes from connected to disconnected
    async fn notify_on_disconnection(connection_monitor: &SessionConnectionMonitor) {
        connection_monitor.connected().await;
        connection_monitor.disconnected().await;
    }

    async fn receive_key_notification_loop(
        shutdown_notifier: Arc<Notify>,
        mut receiver: telemetry::Receiver<state_store::resp3::Operation, C>,
        notification_dispatcher: Arc<
            Dispatcher<(state_store::KeyNotification, Option<AckToken>), String>,
        >,
        connection_monitor: SessionConnectionMonitor,
    ) {
        let mut shutdown_attempt_count = 0;
        loop {
            tokio::select! {
                  // on shutdown/drop, we will be notified so that we can stop receiving any more messages
                  // The loop will continue to receive any more publishes that are already in the queue
                  () = shutdown_notifier.notified() => {
                    match receiver.shutdown().await {
                        Ok(()) => {
                            log::info!("Telemetry Receiver shutdown");
                        }
                        Err(e) => {
                            log::error!("Error shutting down Telemetry Receiver: {e}");
                            // try shutdown again, but not indefinitely
                            if shutdown_attempt_count < 3 {
                                shutdown_attempt_count += 1;
                                shutdown_notifier.notify_one();
                            }
                        }
                    }
                  },
                  () = Self::notify_on_disconnection(&connection_monitor) => {
                    log::warn!("Session disconnected. Dropping key observations as they won't receive any more notifications and must be recreated");
                    // This closes all associated notification channels
                    notification_dispatcher.unregister_all();
                  },
                  msg = receiver.recv() => {
                    if let Some(m) = msg {
                        match m {
                            Ok((notification, ack_token)) => {
                                let Some(key_name) = notification.topic_tokens.get("encodedKeyName") else {
                                    log::error!("Key Notification missing encodedKeyName topic token.");
                                    continue;
                                };
                                let decoded_key_name = HEXUPPER.decode(key_name.as_bytes()).unwrap();
                                let Some(notification_timestamp) = notification.timestamp else {
                                    log::error!("Received key notification with no version. Ignoring.");
                                    continue;
                                };
                                let key_notification = state_store::KeyNotification {
                                    key: decoded_key_name,
                                    operation: notification.payload.clone(),
                                    version: notification_timestamp,
                                };

                                // Try to send the notification to the associated receiver
                                match notification_dispatcher.dispatch(key_name, (key_notification.clone(), ack_token)) {
                                    Ok(()) => {
                                        log::debug!("Key Notification dispatched: {key_notification:?}");
                                    }

                                    Err(DispatchError { data, kind: DispatchErrorKind::SendError }) => {
                                        log::warn!("Key Notification Receiver has been dropped. Received Notification: {data:?}");

                                    }
                                    Err(DispatchError { data, kind: DispatchErrorKind::NotFound(receiver_id) }) => {
                                        log::warn!("Key is not being observed. Received Notification: {data:?} for {receiver_id}");
                                    }
                                }
                            }
                            Err(e) => {
                                // This should only happen on errors subscribing, but it's likely not recoverable
                                log::error!("Error receiving key notifications: {e}. Shutting down Telemetry Receiver.");
                                // try to shutdown telemetry receiver, but not indefinitely
                                if shutdown_attempt_count < 3 {
                                    shutdown_notifier.notify_one();
                                }
                            }
                        }
                    } else {
                        log::info!("Telemetry Receiver closed, no more Key Notifications will be received");
                        // Unregister all receivers, closing the associated channels
                        notification_dispatcher.unregister_all();
                        break;
                    }
                }
            }
        }
    }
}

impl<C> Drop for Client<C>
where
    C: ManagedClient + Clone + Send + Sync,
    C::PubReceiver: Send + Sync,
{
    fn drop(&mut self) {
        self.shutdown_notifier.notify_one();
        log::info!("State Store Client has been dropped.");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    // TODO: This dependency on MqttConnectionSettingsBuilder should be removed in lieu of using a true mock
    use azure_iot_operations_mqtt::MqttConnectionSettingsBuilder;
    use azure_iot_operations_mqtt::session::{Session, SessionOptionsBuilder};
    use azure_iot_operations_protocol::application::ApplicationContextBuilder;

    use crate::state_store::{Error, ErrorKind, SetOptions};

    // TODO: This should return a mock ManagedClient instead.
    // Until that's possible, need to return a Session so that the Session doesn't go out of
    // scope and render the ManagedClient unable to to be used correctly.
    fn create_session() -> Session {
        // TODO: Make a real mock that implements MqttProvider
        let connection_settings = MqttConnectionSettingsBuilder::default()
            .hostname("localhost")
            .client_id("test_client")
            .build()
            .unwrap();
        let session_options = SessionOptionsBuilder::default()
            .connection_settings(connection_settings)
            .build()
            .unwrap();
        Session::new(session_options).unwrap()
    }

    #[tokio::test]
    async fn test_set_empty_key() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .set(
                vec![],
                b"testValue".to_vec(),
                Duration::from_secs(1),
                None,
                SetOptions::default(),
            )
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_get_empty_key() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client.get(vec![], Duration::from_secs(1)).await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_del_empty_key() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .del(vec![], None, Duration::from_secs(1))
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_vdel_empty_key() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .vdel(vec![], b"testValue".to_vec(), None, Duration::from_secs(1))
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_observe_empty_key() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .observe(vec![], Duration::from_secs(1))
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_unobserve_empty_key() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .unobserve(vec![], Duration::from_secs(1))
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_set_invalid_timeout() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .set(
                b"testKey".to_vec(),
                b"testValue".to_vec(),
                Duration::from_secs(0),
                None,
                SetOptions::default(),
            )
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_get_invalid_timeout() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .get(b"testKey".to_vec(), Duration::from_secs(0))
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_del_invalid_timeout() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .del(b"testKey".to_vec(), None, Duration::from_secs(0))
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_vdel_invalid_timeout() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .vdel(
                b"testKey".to_vec(),
                b"testValue".to_vec(),
                None,
                Duration::from_secs(0),
            )
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_observe_invalid_timeout() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .observe(b"testKey".to_vec(), Duration::from_secs(0))
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn test_unobserve_invalid_timeout() {
        let session = create_session();
        let connection_monitor = session.create_connection_monitor();
        let managed_client = session.create_managed_client();
        let state_store_client = super::Client::new(
            ApplicationContextBuilder::default().build().unwrap(),
            managed_client,
            connection_monitor,
            super::ClientOptionsBuilder::default().build().unwrap(),
        )
        .unwrap();
        let response = state_store_client
            .unobserve(b"testKey".to_vec(), Duration::from_secs(0))
            .await;
        assert!(matches!(
            response.unwrap_err(),
            Error(ErrorKind::InvalidArgument(_))
        ));
    }
}
