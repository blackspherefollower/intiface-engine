// Buttplug Rust Source Code File - See https://buttplug.io for more info.
//
// Copyright 2016-2022 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

use buttplug::{
  core::{
    connector::ButtplugConnector,
    errors::ButtplugError,
    message::{
      self,
      //ButtplugDeviceCommandMessageUnion,
      ButtplugClientMessage,
      ButtplugMessage,
      ButtplugMessageValidator,
      ButtplugServerMessage,
    },
  },
  server::{ButtplugServer, ButtplugServerBuilder},
  util::{
    async_manager, device_configuration::UserConfigDeviceIdentifier,
    stream::convert_broadcast_receiver_to_stream,
  },
};
use futures::{future::Future, pin_mut, select, FutureExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, Notify};

// Clone derived here to satisfy tokio broadcast requirements.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ButtplugRemoteServerEvent {
  ClientConnected(String),
  ClientDisconnected,
  DeviceAdded {
    index: u32,
    identifier: UserConfigDeviceIdentifier,
    name: String,
    display_name: Option<String>,
  },
  DeviceRemoved {
    index: u32,
  },
  //DeviceCommand(ButtplugDeviceCommandMessageUnion)
}

#[derive(Error, Debug)]
pub enum ButtplugServerConnectorError {
  #[error("Cannot bring up server for connection: {0}")]
  ConnectorError(String),
}

pub struct ButtplugRemoteServer {
  server: Arc<ButtplugServer>,
  event_sender: broadcast::Sender<ButtplugRemoteServerEvent>,
  disconnect_notifier: Arc<Notify>,
}

async fn run_device_event_stream(
  server: Arc<ButtplugServer>,
  remote_event_sender: broadcast::Sender<ButtplugRemoteServerEvent>,
) {
  let server_receiver = server.event_stream();
  pin_mut!(server_receiver);
  loop { 
    match server_receiver.next().await {
      None => {
        info!("Server disconnected via server disappearance, exiting loop.");
        break;
      }
      Some(msg) => {
        if remote_event_sender.receiver_count() > 0 {
          match &msg {
            ButtplugServerMessage::DeviceAdded(da) => {
              if let Some(device_info) = server.device_manager().device_info(da.device_index()) {
                let added_event = ButtplugRemoteServerEvent::DeviceAdded {
                  index: da.device_index(),
                  name: da.device_name().clone(),
                  identifier: device_info.identifier().clone().into(),
                  display_name: device_info.display_name().clone(),
                };
                if remote_event_sender.send(added_event).is_err() {
                  error!("Cannot send event to owner, dropping and assuming local server thread has exited.");
                }
              }
            }
            ButtplugServerMessage::DeviceRemoved(dr) => {
              let removed_event = ButtplugRemoteServerEvent::DeviceRemoved {
                index: dr.device_index(),
              };
              if remote_event_sender.send(removed_event).is_err() {
                error!("Cannot send event to owner, dropping and assuming local server thread has exited.");
              }
            }
            _ => {}
          }
        }
      }
    }
  }
}

async fn run_server<ConnectorType>(
  server: Arc<ButtplugServer>,
  remote_event_sender: broadcast::Sender<ButtplugRemoteServerEvent>,
  connector: ConnectorType,
  mut connector_receiver: mpsc::Receiver<ButtplugClientMessage>,
  disconnect_notifier: Arc<Notify>,
) where
  ConnectorType: ButtplugConnector<ButtplugServerMessage, ButtplugClientMessage> + 'static,
{
  info!("Starting remote server loop");
  let shared_connector = Arc::new(connector);
  let server_receiver = server.event_stream();
  pin_mut!(server_receiver);
  loop {
    select! {
      connector_msg = connector_receiver.recv().fuse() => match connector_msg {
        None => {
          info!("Connector disconnected, exiting loop.");
          if remote_event_sender.receiver_count() > 0 && remote_event_sender.send(ButtplugRemoteServerEvent::ClientDisconnected).is_err() {
            warn!("Cannot update remote about client disconnection");
          }
          break;
        }
        Some(client_message) => {
          trace!("Got message from connector: {:?}", client_message);
          let server_clone = server.clone();
          let connector_clone = shared_connector.clone();
          let remote_event_sender_clone = remote_event_sender.clone();
          async_manager::spawn(async move {
            if let Err(e) = client_message.is_valid() {
              error!("Message not valid: {:?} - Error: {}", client_message, e);
              let mut err_msg = message::Error::from(ButtplugError::from(e));
              err_msg.set_id(client_message.id());
              connector_clone.send(err_msg.into());
              return;
            }
            match server_clone.parse_message(client_message.clone()).await {
              Ok(ret_msg) => {
                if let ButtplugClientMessage::RequestServerInfo(rsi) = client_message {
                  if remote_event_sender_clone.receiver_count() > 0 && remote_event_sender_clone.send(ButtplugRemoteServerEvent::ClientConnected(rsi.client_name().clone())).is_err() {
                    error!("Cannot send event to owner, dropping and assuming local server thread has exited.");
                  }
                }
                if connector_clone.send(ret_msg).await.is_err() {
                  error!("Cannot send reply to server, dropping and assuming remote server thread has exited.");
                }
              },
              Err(err_msg) => {
                if connector_clone.send(err_msg.into()).await.is_err() {
                  error!("Cannot send reply to server, dropping and assuming remote server thread has exited.");
                }
              }
            }
          });
        }
      },
      _ = disconnect_notifier.notified().fuse() => {
        info!("Server disconnected via controller disappearance, exiting loop.");
        break;
      },
      server_msg = server_receiver.next().fuse() => match server_msg {
        None => {
          info!("Server disconnected via server disappearance, exiting loop.");
          break;
        }
        Some(msg) => {
          if remote_event_sender.receiver_count() > 0 {
            match &msg {
              ButtplugServerMessage::DeviceAdded(da) => {
                if let Some(device_info) = server.device_manager().device_info(da.device_index()) {
                  let added_event = ButtplugRemoteServerEvent::DeviceAdded { index: da.device_index(), name: da.device_name().clone(), identifier: device_info.identifier().clone().into(), display_name: device_info.display_name().clone() };
                  if remote_event_sender.send(added_event).is_err() {
                    error!("Cannot send event to owner, dropping and assuming local server thread has exited.");
                  }
                }
              },
              ButtplugServerMessage::DeviceRemoved(dr) => {
                let removed_event = ButtplugRemoteServerEvent::DeviceRemoved { index: dr.device_index() };
                if remote_event_sender.send(removed_event).is_err() {
                  error!("Cannot send event to owner, dropping and assuming local server thread has exited.");
                }
              },
              _ => {}
            }
          }
          let connector_clone = shared_connector.clone();
          if connector_clone.send(msg).await.is_err() {
            error!("Server disappeared, exiting remote server thread.");
          }
        }
      },
    };
  }
  if let Err(err) = server.disconnect().await {
    error!("Error disconnecting server: {:?}", err);
  }
  info!("Exiting remote server loop");
}

impl Default for ButtplugRemoteServer {
  fn default() -> Self {
    Self::new(
      ButtplugServerBuilder::default()
        .finish()
        .expect("Default is infallible"),
    )
  }
}

impl ButtplugRemoteServer {
  pub fn new(server: ButtplugServer) -> Self {
    let (event_sender, _) = broadcast::channel(256);
    let wrapped_server = Arc::new(server);
    // Thanks to the existence of the backdoor server, device updates can happen for the lifetime to
    // the RemoteServer instance, not just during client connect. We need to make sure these are
    // emitted to the frontend.
    tokio::spawn({
      let server = wrapped_server.clone();
      let event_sender = event_sender.clone();
      async move {
        run_device_event_stream(server, event_sender).await;
      }
    });
    Self {
      event_sender,
      server: wrapped_server.clone(),
      disconnect_notifier: Arc::new(Notify::new()),
    }
  }

  pub fn event_stream(&self) -> impl Stream<Item = ButtplugRemoteServerEvent> {
    convert_broadcast_receiver_to_stream(self.event_sender.subscribe())
  }

  pub fn start<ConnectorType>(
    &self,
    mut connector: ConnectorType,
  ) -> impl Future<Output = Result<(), ButtplugServerConnectorError>>
  where
    ConnectorType: ButtplugConnector<ButtplugServerMessage, ButtplugClientMessage> + 'static,
  {
    let server = self.server.clone();
    let event_sender = self.event_sender.clone();
    let disconnect_notifier = self.disconnect_notifier.clone();
    async move {
      let (connector_sender, connector_receiver) = mpsc::channel(256);
      // Due to the connect method requiring a mutable connector, we must connect before starting up
      // our server loop. Anything that needs to happen outside of the client connection session
      // should happen around this. This flow is locked.
      connector
        .connect(connector_sender)
        .await
        .map_err(|e| ButtplugServerConnectorError::ConnectorError(format!("{:?}", e)))?;
      run_server(
        server,
        event_sender,
        connector,
        connector_receiver,
        disconnect_notifier,
      )
      .await;
      Ok(())
    }
  }

  pub async fn disconnect(&self) -> Result<(), ButtplugError> {
    self.disconnect_notifier.notify_waiters();
    Ok(())
  }

  pub async fn shutdown(&self) -> Result<(), ButtplugError> {
    self.server.shutdown().await?;
    Ok(())
  }
}

impl Drop for ButtplugRemoteServer {
  fn drop(&mut self) {
    self.disconnect_notifier.notify_waiters();
  }
}
