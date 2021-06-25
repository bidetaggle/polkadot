// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.


use std::time::SystemTime;

/// Sending and receiving of `DisputeRequest`s.

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt, TryFutureExt};

use sp_keystore::SyncCryptoStorePtr;

use polkadot_node_primitives::DISPUTE_WINDOW;
use polkadot_subsystem::messages::{AllMessages, NetworkBridgeMessage};
use polkadot_subsystem::{
	messages::DisputeDistributionMessage, FromOverseer, OverseerSignal, SpawnedSubsystem,
	Subsystem, SubsystemContext, SubsystemError,
};
use polkadot_node_subsystem_util::{
	runtime,
	runtime::RuntimeInfo,
};

/// `DisputeSender` manages the sending side of all initiated disputes.
///
/// See the implementers guide for more detail on the general protocol, but in general
/// `DisputeSender` takes care of getting our vote out to all other relevant validators.
mod sender;
use self::sender::{DisputeSender, FromSendingTask};

/// Handle receival of dispute requests.
///
/// - Spam/Flood handling
/// - Trigger import of statements
mod receiver;
use self::receiver::DisputesReceiver;

/// Error and [`Result`] type for this subsystem.
mod error;
use error::{Fatal, FatalResult};
use error::{Result, log_error};

#[cfg(test)]
mod tests;

// mod metrics;
//// Prometheus `Metrics` for dispute distribution.
// pub use metrics::Metrics;

// #[cfg(test)]
// mod tests;

const LOG_TARGET: &'static str = "parachain::dispute-distribution";

/// The dispute distribution subsystem.
pub struct DisputeDistributionSubsystem {
	/// Easy and efficient runtime access for this subsystem.
	runtime: RuntimeInfo,

	/// Sender for our dispute requests.
	disputes_sender: DisputeSender,

	/// Receive messages from `SendTask`.
	sender_rx: mpsc::Receiver<FromSendingTask>,
}

impl<Context> Subsystem<Context> for DisputeDistributionSubsystem
where
	Context: SubsystemContext<Message = DisputeDistributionMessage> + Sync + Send,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = self
			.run(ctx)
			.map_err(|e| SubsystemError::with_origin("dispute-distribution", e))
			.boxed();

		SpawnedSubsystem {
			name: "dispute-distribution-subsystem",
			future,
		}
	}
}

impl DisputeDistributionSubsystem {

	/// Create a new instance of the availability distribution.
	pub fn new(keystore: SyncCryptoStorePtr) -> Self {
		let runtime = RuntimeInfo::new_with_config(runtime::Config {
			keystore: Some(keystore),
			session_cache_lru_size: DISPUTE_WINDOW as usize,
		});
		let (tx, sender_rx) = mpsc::channel(1);
		let disputes_sender = DisputeSender::new(tx);
		Self { runtime, disputes_sender, sender_rx }
	}

	/// Start processing work as passed on from the Overseer.
	async fn run<Context>(mut self, mut ctx: Context) -> std::result::Result<(), Fatal>
	where
		Context: SubsystemContext<Message = DisputeDistributionMessage> + Sync + Send,
	{
		let mut start_processing = SystemTime::now();
		loop {
			let now = SystemTime::now();
			tracing::trace!(
				target: LOG_TARGET,
				elapsed = ?start_processing.elapsed().unwrap().as_millis(),
				"Waiting for message"
			);
			let message = Message::receive(&mut ctx, &mut self.sender_rx).await;
			tracing::trace!(
				target: LOG_TARGET,
				elapsed = ?now.elapsed().unwrap().as_millis(),
				?message,
				"Got message"
			);
			start_processing = SystemTime::now();
			match message {
				Message::Subsystem(result) => {
					let result = match result? {
						FromOverseer::Signal(signal) => {
							match self.handle_signals(&mut ctx, signal).await {
								SignalResult::Conclude => return Ok(()),
								SignalResult::Result(result) => result,
							}
						}
						FromOverseer::Communication { msg } =>
							self.handle_subsystem_message(&mut ctx, msg).await,
					};
					log_error(result, "on FromOverseer")?;
				}
				Message::Sender(result) => {
					self.disputes_sender.on_task_message(
						result.ok_or(Fatal::SenderExhausted)?
					)
					.await;
				}
			}
		}
	}

	/// Handle overseer signals.
	async fn handle_signals<Context: SubsystemContext> (
		&mut self,
		ctx: &mut Context,
		signal: OverseerSignal
	) -> SignalResult
	{
		let result = match signal {
			OverseerSignal::Conclude => return SignalResult::Conclude,
			OverseerSignal::ActiveLeaves(update) => {
				self.disputes_sender.update_leaves(
					ctx,
					&mut self.runtime,
					update
				)
				.await
				.map_err(From::from)
			}
			OverseerSignal::BlockFinalized(_,_) => {
				Ok(())
			}
		};
		SignalResult::Result(result)
	}

	/// Handle `DisputeDistributionMessage`s.
	async fn handle_subsystem_message<Context: SubsystemContext> (
		&mut self,
		ctx: &mut Context,
		msg: DisputeDistributionMessage
	) -> Result<()>
	{
		match msg {
			DisputeDistributionMessage::SendDispute(dispute_msg) =>
				self.disputes_sender.start_sending(ctx, &mut self.runtime, dispute_msg).await?,
			// This message will only arrive once:
			DisputeDistributionMessage::DisputeSendingReceiver(receiver) => {
				let (tx, rx) = oneshot::channel();
				let now = SystemTime::now();
				ctx.send_message(
					AllMessages::NetworkBridge(
						NetworkBridgeMessage::GetAuthorityDiscoveryService(tx)
					)
				).await;
				let service = rx
					.await
					.map_err(|_| Fatal::CanceledOneshot("get_authority_discovery_service"))?;
				tracing::trace!(
					target: LOG_TARGET,
					elapsed = ?now.elapsed().unwrap().as_millis(),
					"send_message + rx.await"
				);

				let receiver = DisputesReceiver::new(ctx.sender().clone(), receiver, service);
				ctx
					.spawn("disputes-receiver", receiver.run().boxed(),)
					.await
					.map_err(Fatal::SpawnTask)?;
			},

		}
		Ok(())
	}
}

/// Messages to be handled in this subsystem.
#[derive(Debug)]
enum Message {
	/// Messages from other subsystems.
	Subsystem(FatalResult<FromOverseer<DisputeDistributionMessage>>),
	/// Messages from spawned sender background tasks.
	Sender(Option<FromSendingTask>),
}

impl Message {
	async fn receive(
		ctx: &mut impl SubsystemContext<Message = DisputeDistributionMessage>,
		from_sender: &mut mpsc::Receiver<FromSendingTask>,
	) -> Message {
		// We are only fusing here to make `select` happy, in reality we will quit if the stream
		// ends.
		let from_overseer = ctx.recv().fuse();
		futures::pin_mut!(from_overseer, from_sender);
		futures::select!(
			msg = from_overseer => Message::Subsystem(msg.map_err(Fatal::SubsystemReceive)),
			msg = from_sender.next() => Message::Sender(msg),
		)
	}
}

/// Result of handling signal from overseer.
enum SignalResult {
	Conclude,
	Result(Result<()>),
}
