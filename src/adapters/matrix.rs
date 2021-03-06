use crate::event::{ExternalMessage, ExternalOrigin};
use crate::manager::{FieldAddress, ProvidedMessage, ProvidedMessagePart};
use crate::Result;
use async_channel::{Receiver, Sender};
use matrix_sdk::events::room::member::MemberEventContent;
use matrix_sdk::events::room::message::MessageEventContent;
use matrix_sdk::events::{StrippedStateEvent, SyncMessageEvent};

use matrix_sdk::{Client, ClientConfig, EventEmitter, RoomState, SyncSettings};

use tokio::time::{self, Duration};
use url::Url;

const REJOIN_DELAY: u64 = 3;
const REJOIN_MAX_ATTEMPTS: usize = 5;

// TODO: This type should be unified with other adapters.
pub struct MatrixMessage {
    from: String,
    message: String,
}

impl From<MatrixMessage> for ExternalMessage {
    fn from(val: MatrixMessage) -> Self {
        ExternalMessage {
            origin: ExternalOrigin::Matrix,
            field_address: FieldAddress::from(val.from),
            message: ProvidedMessage {
                parts: vec![ProvidedMessagePart::from(val.message)],
            },
        }
    }
}

#[derive(Clone)]
pub struct MatrixClient {
    client: Client, // `Client` from matrix_sdk
    sender: Sender<MatrixMessage>,
}

impl MatrixClient {
    pub async fn new(
        homeserver: &str,
        username: &str,
        password: &str,
        db_path: &str,
    ) -> Result<(MatrixClient, Receiver<MatrixMessage>)> {
        info!("Setting up Matrix client");
        // Setup client
        let client_config = ClientConfig::new().store_path(db_path);

        let homeserver = Url::parse(homeserver).expect("Couldn't parse the homeserver URL");
        let client = Client::new_with_config(homeserver, client_config)?;

        // Login with credentials
        client
            .login(username, password, None, Some("w3f-registrar-bot"))
            .await?;

        // Sync up, avoid responding to old messages.
        info!("Syncing Matrix client");
        client.sync(SyncSettings::default()).await;

        let (tx, recv) = async_channel::unbounded();

        Ok((
            MatrixClient {
                client: client,
                sender: tx,
            },
            recv,
        ))
    }
    pub async fn start(&self) {
        self.client.add_event_emitter(Box::new(self.clone())).await;
    }
}

#[async_trait]
impl EventEmitter for MatrixClient {
    async fn on_stripped_state_member(
        &self,
        room: RoomState,
        _: &StrippedStateEvent<MemberEventContent>,
        _: Option<MemberEventContent>,
    ) {
        if let RoomState::Invited(room) = room {
            let mut delay = REJOIN_DELAY;
            let mut rejoin_attempts = 0;

            while let Err(err) = self.client.join_room_by_id(room.room_id()).await {
                warn!(
                    "Failed to join room {} ({:?}), retrying in {}s",
                    room.room_id(),
                    err,
                    delay,
                );

                time::sleep(Duration::from_secs(delay)).await;
                delay *= 2;
                rejoin_attempts += 1;

                if rejoin_attempts == REJOIN_MAX_ATTEMPTS {
                    error!("Can't join room {} ({:?})", room.room_id(), err);
                    return;
                }
            }

            debug!("Joined room {}", room.room_id());
        }
    }
    async fn on_room_message(
        &self,
        room: RoomState,
        event: &SyncMessageEvent<MessageEventContent>,
    ) {
        if let RoomState::Joined(_) = room {
            match event.content {
                MessageEventContent::Text(ref content) => {
                    debug!(
                        "Received message \"{}\" from {}",
                        content.body, event.sender
                    );

                    // Send the message to `crate::system`, where the message
                    // will be processed by an aggregate and sent to the event
                    // store.
                    let _ = self
                        .sender
                        .send(MatrixMessage {
                            from: event.sender.to_string(),
                            message: content.body.clone(),
                        })
                        .await
                        .map_err(|err| {
                            error!(
                                "Failed to send message from Matrix adapter to system: {:?}",
                                err
                            )
                        });
                }
                _ => {
                    trace!("Received unacceptable message type from {}", event.sender);
                }
            }
        }
    }
}
