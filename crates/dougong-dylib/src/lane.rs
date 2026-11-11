//! Per-activation kernel lanes: the plain threads that run dylib FFI
//! exports off runtime workers.
//!
//! The 2026-11-12 fly-rafter wedge: a tokio worker executing a dispatch
//! task blocked inside `drive`'s hand-rolled `block_on` while its handler
//! waited on an instance lock whose granted waiter had been scheduled into
//! that same worker's private LIFO slot (tokio's `schedule_local` routes
//! by the waking thread, not the task owner) — reachable only when the
//! frozen run loop resumed, which required the very wait it was parked in.
//! Root fix: FFI export execution never runs on a runtime worker. Every
//! activation owns one lane thread (plain, `enter_kernel_lane`-marked);
//! every FFI segment (`setup`, `call_full`, `dispatch_event`,
//! `deliver_next`, `teardown`) marshals onto it and the caller awaits the
//! reply asynchronously. A lane's own `block_on` parks are sanctioned (it
//! has no run loop to starve) and its task wake-ups route through the
//! inject queue, so the LIFO capture becomes structurally impossible.

use std::sync::mpsc::{Sender, channel};

use crate::{Error, enter_kernel_lane};

/// One unit of lane work: a prepared FFI segment whose reply travels on
/// the caller's own oneshot channel.
type LaneJob = Box<dyn FnOnce() + Send + 'static>;

/// Messages the lane loop accepts: one FFI segment, or the teardown close
/// pill that ends the thread after the queued segments drain.
enum LaneMsg {
    Job(LaneJob),
    Close,
}

/// Caller side of one activation's lane: a cloneable job submitter. Every
/// clone dies with the kernel registrations it belongs to; the activation
/// teardown closes the lane explicitly through [`close`].
#[derive(Clone)]
pub struct Lane {
    tx: Sender<LaneMsg>,
}

impl Lane {
    /// Submits the FFI segment `job` to the lane and returns the reply
    /// receiver. The job result may be any Send value the caller awaits;
    /// a receive error means the lane thread died (a panicking segment is
    /// caught, so this surfaces lane-level failures only).
    pub fn execute<R, F>(&self, job: F) -> tokio::sync::oneshot::Receiver<R>
    where
        R: Send + 'static,
        F: FnOnce() -> R + Send + 'static,
    {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(LaneMsg::Job(Box::new(move || {
            // A failed reply send means the caller stopped awaiting (its
            // future was dropped); nothing to recover.
            let _ = reply_tx.send(job());
        })));
        reply_rx
    }
}

/// Spawns one activation's lane thread. The thread runs every submitted
/// segment to completion, drains ahead of the close pill, and acknowledges
/// its exit on the returned receiver.
///
/// # Errors
///
/// Fails when the thread cannot be spawned; the activation then fails to
/// apply.
pub fn spawn(instance_id: u64) -> Result<(Lane, tokio::sync::oneshot::Receiver<()>), Error> {
    let (tx, rx) = channel::<LaneMsg>();
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("dougong-lane-{instance_id}"))
        .spawn(move || {
            // The lane is the sanctioned blocking-drive context: plain
            // thread, no runtime run loop, so its parks own their waits
            // (the `runtime_starvation` guard's documented exemption).
            let _lane_marker = enter_kernel_lane();
            while let Ok(msg) = rx.recv() {
                match msg {
                    LaneMsg::Job(job) => {
                        // A panicking segment must not kill the lane; the
                        // caller observes the dropped reply channel.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                    LaneMsg::Close => break,
                }
            }
            let _ = ack_tx.send(());
        });
    match spawned {
        Ok(_join) => Ok((Lane { tx }, ack_rx)),
        Err(err) => Err(Error::transport(
            "lane_spawn",
            format!("spawn activation lane: {err}"),
        )),
    }
}

/// Closes the lane: sends the close pill (queued segments drain ahead of
/// it, FIFO) and awaits the lane's exit acknowledgement, keeping the
/// disposer asynchronous — no blocking join on a runtime worker.
///
/// # Errors
///
/// Fails when the lane is already closed or its thread died before the
/// acknowledgement.
pub async fn close(lane: Lane, ack: tokio::sync::oneshot::Receiver<()>) -> Result<(), Error> {
    lane.tx.send(LaneMsg::Close).map_err(|err| {
        Error::transport(
            "lane_closed",
            format!("activation lane is already closed: {err}"),
        )
    })?;
    ack.await.map_err(|err| {
        Error::transport(
            "lane_closed",
            format!("activation lane died before the exit ack: {err}"),
        )
    })
}
