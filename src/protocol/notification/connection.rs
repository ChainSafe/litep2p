// Copyright 2023 litep2p developers
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::{
    protocol::notification::handle::NotificationEventHandle, substream::Substream, PeerId,
};

use bytes::BytesMut;
use futures::{FutureExt, SinkExt, Stream, StreamExt};
use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot,
};
use tokio_util::sync::PollSender;

use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::notification::connection";

/// Bidirectional substream pair representing a connection to a remote peer.
pub(crate) struct Connection {
    /// Remote peer ID.
    peer: PeerId,

    /// Inbound substreams for receiving notifications.
    inbound: Substream,

    /// Outbound substream for sending notifications.
    outbound: Substream,

    /// Handle for sending notification events to user.
    event_handle: NotificationEventHandle,

    /// TX channel used to notify [`NotificationProtocol`](super::NotificationProtocol)
    /// that the connection has been closed.
    conn_closed_tx: Sender<PeerId>,

    /// TX channel for sending notifications.
    notif_tx: PollSender<(PeerId, BytesMut)>,

    /// Receiver for asynchronously sent notifications.
    async_rx: Receiver<Vec<u8>>,

    /// Receiver for synchronously sent notifications.
    sync_rx: Receiver<Vec<u8>>,

    /// Oneshot receiver used by [`NotificationProtocol`](super::NotificationProtocol)
    /// to signal that local node wishes the close the connection.
    rx: oneshot::Receiver<()>,

    /// Next notification to send, if any.
    next_notification: Option<Vec<u8>>,

    /// Whether the inbound (read) side of the connection has been closed by the remote.
    ///
    /// When the remote peer half-closes its write side (e.g., sends FIN on WebRTC), the inbound
    /// stream will yield `None`. Rather than tearing down the entire connection, we mark the read
    /// side as closed and continue operating in write-only mode. This is important for notification
    /// substreams where the remote may have nothing to send (e.g., a light client during warp sync)
    /// but still needs to receive notifications.
    read_closed: bool,
}

/// Notify [`NotificationProtocol`](super::NotificationProtocol) that the connection was closed.
#[derive(Debug)]
pub enum NotifyProtocol {
    /// Notify the protocol handler.
    Yes,

    /// Do not notify protocol handler.
    No,
}

impl Connection {
    /// Create new [`Connection`].
    pub(crate) fn new(
        peer: PeerId,
        inbound: Substream,
        outbound: Substream,
        event_handle: NotificationEventHandle,
        conn_closed_tx: Sender<PeerId>,
        notif_tx: Sender<(PeerId, BytesMut)>,
        async_rx: Receiver<Vec<u8>>,
        sync_rx: Receiver<Vec<u8>>,
    ) -> (Self, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel();

        (
            Self {
                rx,
                peer,
                sync_rx,
                async_rx,
                inbound,
                outbound,
                event_handle,
                conn_closed_tx,
                next_notification: None,
                notif_tx: PollSender::new(notif_tx),
                read_closed: false,
            },
            tx,
        )
    }

    /// Connection closed, clean up state.
    ///
    /// If [`NotificationProtocol`](super::NotificationProtocol) was the one that initiated
    /// shut down, it's not notified of connection getting closed.
    async fn close_connection(self, notify_protocol: NotifyProtocol) {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?notify_protocol,
            "close notification protocol",
        );

        let _ = self.inbound.close().await;
        let _ = self.outbound.close().await;

        if std::matches!(notify_protocol, NotifyProtocol::Yes) {
            let _ = self.conn_closed_tx.send(self.peer).await;
        }

        self.event_handle.report_notification_stream_closed(self.peer).await;
    }

    pub async fn start(mut self) {
        tracing::debug!(
            target: LOG_TARGET,
            peer = ?self.peer,
            "start connection event loop",
        );

        loop {
            match self.next().await {
                None
                | Some(ConnectionEvent::CloseConnection {
                    notify: NotifyProtocol::Yes,
                }) => return self.close_connection(NotifyProtocol::Yes).await,
                Some(ConnectionEvent::CloseConnection {
                    notify: NotifyProtocol::No,
                }) => return self.close_connection(NotifyProtocol::No).await,
                Some(ConnectionEvent::NotificationReceived { notification }) => {
                    if let Err(_) = self.notif_tx.send_item((self.peer, notification)) {
                        return self.close_connection(NotifyProtocol::Yes).await;
                    }
                }
            }
        }
    }
}

/// Connection events.
pub enum ConnectionEvent {
    /// Close connection.
    ///
    /// If `NotificationProtocol` requested [`Connection`] to be closed, it doesn't need to be
    /// notified. If, on the other hand, connection closes because it encountered an error or one
    /// of the substreams was closed, `NotificationProtocol` must be informed so it can inform the
    /// user.
    CloseConnection {
        /// Whether to notify `NotificationProtocol` or not.
        notify: NotifyProtocol,
    },

    /// Notification read from the inbound substream.
    ///
    /// NOTE: [`Connection`] uses `PollSender::send_item()` to send the notification to user.
    /// `PollSender::poll_reserve()` must be called before calling `PollSender::send_item()` or it
    /// will panic. `PollSender::poll_reserve()` is called in the `Stream` implementation below
    /// before polling the inbound substream to ensure the channel has capacity to receive a
    /// notification.
    NotificationReceived {
        /// Notification.
        notification: BytesMut,
    },
}

impl Connection {
    #[cfg(test)]
    fn is_read_closed(&self) -> bool {
        self.read_closed
    }
}

impl Stream for Connection {
    type Item = ConnectionEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        if let Poll::Ready(_) = this.rx.poll_unpin(cx) {
            return Poll::Ready(Some(ConnectionEvent::CloseConnection {
                notify: NotifyProtocol::No,
            }));
        }

        loop {
            let notification = match this.next_notification.take() {
                Some(notification) => Some(notification),
                None => {
                    let future = async {
                        tokio::select! {
                            notification = this.async_rx.recv() => notification,
                            notification = this.sync_rx.recv() => notification,
                        }
                    };
                    futures::pin_mut!(future);

                    match future.poll_unpin(cx) {
                        Poll::Pending => None,
                        Poll::Ready(None) =>
                            return Poll::Ready(Some(ConnectionEvent::CloseConnection {
                                notify: NotifyProtocol::Yes,
                            })),
                        Poll::Ready(Some(notification)) => Some(notification),
                    }
                }
            };

            let Some(notification) = notification else {
                break;
            };

            match this.outbound.poll_ready_unpin(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Pending => {
                    this.next_notification = Some(notification);
                    break;
                }
                Poll::Ready(Err(_)) =>
                    return Poll::Ready(Some(ConnectionEvent::CloseConnection {
                        notify: NotifyProtocol::Yes,
                    })),
            }

            if let Err(_) = this.outbound.start_send_unpin(notification.into()) {
                return Poll::Ready(Some(ConnectionEvent::CloseConnection {
                    notify: NotifyProtocol::Yes,
                }));
            }
        }

        match this.outbound.poll_flush_unpin(cx) {
            Poll::Ready(Err(_)) =>
                return Poll::Ready(Some(ConnectionEvent::CloseConnection {
                    notify: NotifyProtocol::Yes,
                })),
            Poll::Ready(Ok(())) | Poll::Pending => {}
        }

        // If the remote has half-closed the inbound stream (e.g., WebRTC FIN), skip reading.
        // The connection continues in write-only mode — outbound notifications can still be sent.
        if this.read_closed {
            return Poll::Pending;
        }

        if let Err(_) = futures::ready!(this.notif_tx.poll_reserve(cx)) {
            return Poll::Ready(Some(ConnectionEvent::CloseConnection {
                notify: NotifyProtocol::Yes,
            }));
        }

        match futures::ready!(this.inbound.poll_next_unpin(cx)) {
            None | Some(Err(_)) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    peer = ?this.peer,
                    "inbound stream closed by remote (half-close), continuing in write-only mode",
                );
                this.read_closed = true;
                // Re-register wakers by going through the poll cycle again so we remain
                // responsive to outbound work and local shutdown signals.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Some(Ok(notification)) =>
                Poll::Ready(Some(ConnectionEvent::NotificationReceived { notification })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mock::substream::MockSubstream,
        protocol::notification::{handle::NotificationEventHandle, types::InnerNotificationEvent},
        substream::Substream as GenericSubstream,
        types::SubstreamId,
    };
    use futures::StreamExt;
    use tokio::sync::mpsc::channel;

    /// Helper to create a `Connection` with mock substreams.
    ///
    /// Returns the connection plus channels for driving and observing behavior:
    /// - `async_tx`: send outbound notifications to the connection
    /// - `conn_closed_rx`: receives peer ID when connection closes
    /// - `event_rx`: receives notification events reported to the user
    /// - `shutdown_tx`: oneshot to signal local-initiated close
    /// Channels that must be kept alive for the connection to function but aren't
    /// directly needed by the tests.
    #[allow(dead_code)]
    struct ConnectionBackground {
        sync_tx: Sender<Vec<u8>>,
        notif_rx: tokio::sync::mpsc::Receiver<(PeerId, BytesMut)>,
    }

    fn make_connection(
        inbound: MockSubstream,
        outbound: MockSubstream,
    ) -> (
        Connection,
        Sender<Vec<u8>>,
        ConnectionBackground,
        tokio::sync::mpsc::Receiver<PeerId>,
        tokio::sync::mpsc::Receiver<InnerNotificationEvent>,
        oneshot::Sender<()>,
    ) {
        let peer = PeerId::random();
        let (conn_closed_tx, conn_closed_rx) = channel(8);
        let (notif_tx, notif_rx) = channel(64);
        let (async_tx, async_rx) = channel(64);
        let (sync_tx, sync_rx) = channel(64);
        let (event_tx, event_rx) = channel(64);
        let event_handle = NotificationEventHandle::new(event_tx);

        let inbound_substream =
            GenericSubstream::new_mock(peer, SubstreamId::from(0usize), Box::new(inbound));
        let outbound_substream =
            GenericSubstream::new_mock(peer, SubstreamId::from(1usize), Box::new(outbound));

        let (conn, shutdown_tx) = Connection::new(
            peer,
            inbound_substream,
            outbound_substream,
            event_handle,
            conn_closed_tx,
            notif_tx,
            async_rx,
            sync_rx,
        );

        let bg = ConnectionBackground { sync_tx, notif_rx };
        (conn, async_tx, bg, conn_closed_rx, event_rx, shutdown_tx)
    }

    #[tokio::test]
    async fn half_close_does_not_tear_down_connection() {
        // Inbound returns None immediately (simulates remote FIN / half-close).
        let mut inbound = MockSubstream::new();
        inbound
            .expect_poll_next()
            .returning(|_| Poll::Ready(None));

        // Outbound accepts writes and flushes successfully.
        let mut outbound = MockSubstream::new();
        outbound
            .expect_poll_ready()
            .returning(|_| Poll::Ready(Ok(())));
        outbound
            .expect_start_send()
            .returning(|_| Ok(()));
        outbound
            .expect_poll_flush()
            .returning(|_| Poll::Ready(Ok(())));

        let (mut conn, async_tx, _bg, mut conn_closed_rx, _event_rx, _shutdown_tx) =
            make_connection(inbound, outbound);

        // Verify read_closed was set by polling once.
        futures::future::poll_fn(|cx| {
            // This first poll should process the inbound None and set read_closed.
            match conn.poll_next_unpin(cx) {
                Poll::Pending => {
                    assert!(conn.is_read_closed(), "read_closed should be set after inbound returns None");
                    Poll::Ready(())
                }
                Poll::Ready(Some(ConnectionEvent::CloseConnection { .. })) => {
                    panic!("Connection should NOT close on remote half-close");
                }
                Poll::Ready(other) => {
                    panic!("Unexpected event: {:?}", other.is_some());
                }
            }
        })
        .await;

        // Verify the connection was NOT reported as closed.
        assert!(
            conn_closed_rx.try_recv().is_err(),
            "Connection should not be reported as closed"
        );

        // Now send an outbound notification — the write side should still work.
        async_tx.send(vec![1, 2, 3]).await.unwrap();

        // Poll the connection again — it should process the outbound notification
        // without trying to read from inbound.
        futures::future::poll_fn(|cx| {
            match conn.poll_next_unpin(cx) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(Some(ConnectionEvent::CloseConnection { .. })) => {
                    panic!("Connection should NOT close after sending outbound notification");
                }
                _ => Poll::Ready(()),
            }
        })
        .await;

        // Verify the connection is still alive (no close notification sent).
        assert!(
            conn_closed_rx.try_recv().is_err(),
            "Connection should still be alive after outbound notification"
        );
    }

    #[tokio::test]
    async fn half_close_connection_closes_on_local_shutdown() {
        // Inbound returns None immediately (simulates remote FIN / half-close).
        let mut inbound = MockSubstream::new();
        inbound
            .expect_poll_next()
            .returning(|_| Poll::Ready(None));

        // Outbound accepts writes.
        let mut outbound = MockSubstream::new();
        outbound
            .expect_poll_ready()
            .returning(|_| Poll::Pending);
        outbound
            .expect_poll_flush()
            .returning(|_| Poll::Ready(Ok(())));
        outbound
            .expect_poll_close()
            .returning(|_| Poll::Ready(Ok(())));

        let (mut conn, _async_tx, _bg, _conn_closed_rx, _event_rx, shutdown_tx) =
            make_connection(inbound, outbound);

        // First, let read_closed get set.
        futures::future::poll_fn(|cx| {
            let _ = conn.poll_next_unpin(cx);
            if conn.is_read_closed() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        assert!(conn.is_read_closed());

        // Now trigger local shutdown.
        shutdown_tx.send(()).unwrap();

        // The connection should emit CloseConnection with NotifyProtocol::No.
        let event = futures::future::poll_fn(|cx| conn.poll_next_unpin(cx)).await;

        assert!(
            matches!(
                event,
                Some(ConnectionEvent::CloseConnection {
                    notify: NotifyProtocol::No
                })
            ),
            "Local shutdown should close connection with NotifyProtocol::No"
        );
    }
}
