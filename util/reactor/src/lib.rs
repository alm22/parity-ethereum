// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Tokio Core Reactor wrapper.

extern crate futures;
extern crate tokio;

use std::{fmt, thread};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use futures::{future, Future, IntoFuture};
pub use tokio::timer::Delay;
pub use tokio::runtime::{Runtime, TaskExecutor};

/// Event Loop for futures.
///
/// Runs in a separate thread.
pub struct EventLoop {
	remote: Remote,
	handle: EventLoopHandle,
}

impl EventLoop {
	/// Spawns a new thread with `EventLoop` with given handler.
	pub fn spawn() -> Self {
		let (stop, stopped) = futures::oneshot();
		let (tx, rx) = mpsc::channel();
		let handle = thread::spawn(move || {
			let mut runtime = Runtime::new().expect("Creating an event loop should not fail.");
			tx.send(runtime.executor()).expect("Rx is blocking upper thread.");
			runtime.spawn(futures::empty().select(stopped).map(|_| ()).map_err(|_| ()));
			runtime.shutdown_on_idle().wait().expect("Tokio runtime shutdown should not fail.");
		});
		let remote = rx.recv().expect("tx is transfered to a newly spawned thread.");

		EventLoop {
			remote: Remote {
				inner: Mode::Tokio(remote),
			},
			handle: EventLoopHandle {
				close: Some(stop),
				handle: Some(handle),
			},
		}
	}

	/// Returns this event loop raw executor.
	///
	/// Deprecated: Exists only to connect with current JSONRPC implementation.
	pub fn raw_executor(&self) -> TaskExecutor {
		if let Mode::Tokio(ref executor) = self.remote.inner {
			executor.clone()
		} else {
			panic!("Event loop is never initialized in other mode then Tokio.")
		}
	}

	/// Returns event loop remote.
	pub fn remote(&self) -> Remote {
		self.remote.clone()
	}
}

#[derive(Clone)]
enum Mode {
	Tokio(TaskExecutor),
	Sync,
	ThreadPerFuture,
}

impl fmt::Debug for Mode {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		use self::Mode::*;

		match *self {
			Tokio(_) => write!(fmt, "tokio"),
			Sync => write!(fmt, "synchronous"),
			ThreadPerFuture => write!(fmt, "thread per future"),
		}
	}
}

/// Returns a future which runs `f` until `duration` has elapsed, at which
/// time `on_timeout` is run and the future resolves.
fn timeout<F, R, T>(f: F, duration: Duration, on_timeout: T)
	-> impl Future<Item = (), Error = ()> + Send + 'static
where
	T: FnOnce() -> () + Send + 'static,
	F: FnOnce() -> R + Send + 'static,
	R: IntoFuture<Item=(), Error=()> + Send + 'static,
	R::Future: Send + 'static,
{
	let future = future::lazy(f);
	let timeout = Delay::new(Instant::now() + duration)
		.then(move |_| {
			on_timeout();
			Ok(())
		});
	future.select(timeout).then(|_| Ok(()))
}

#[derive(Debug, Clone)]
pub struct Remote {
	inner: Mode,
}

impl Remote {
	/// Remote for existing event loop.
	///
	/// Deprecated: Exists only to connect with current JSONRPC implementation.
	pub fn new(remote: TaskExecutor) -> Self {
		Remote {
			inner: Mode::Tokio(remote),
		}
	}

	/// Synchronous remote, used mostly for tests.
	pub fn new_sync() -> Self {
		Remote {
			inner: Mode::Sync,
		}
	}

	/// Spawns a new thread for each future (use only for tests).
	pub fn new_thread_per_future() -> Self {
		Remote {
			inner: Mode::ThreadPerFuture,
		}
	}

	/// Spawn a future to this event loop
	pub fn spawn<R>(&self, r: R) where
        R: IntoFuture<Item=(), Error=()> + Send + 'static,
        R::Future: Send + 'static,
	{
		match self.inner {
			Mode::Tokio(ref remote) => remote.spawn(r.into_future()),
			Mode::Sync => {
				let _= r.into_future().wait();
			},
			Mode::ThreadPerFuture => {
				thread::spawn(move || {
					let _= r.into_future().wait();
				});
			},
		}
	}

	/// Spawn a new future returned by given closure.
	pub fn spawn_fn<F, R>(&self, f: F) where
		F: FnOnce() -> R + Send + 'static,
        R: IntoFuture<Item=(), Error=()> + Send + 'static,
        R::Future: Send + 'static,
	{
		match self.inner {
			Mode::Tokio(ref remote) => remote.spawn(future::lazy(f)),
			Mode::Sync => {
				let _ = future::lazy(f).wait();
			},
			Mode::ThreadPerFuture => {
				thread::spawn(move || {
					let _= f().into_future().wait();
				});
			},
		}
	}

	/// Spawn a new future and wait for it or for a timeout to occur.
	pub fn spawn_with_timeout<F, R, T>(&self, f: F, duration: Duration, on_timeout: T) where
		T: FnOnce() -> () + Send + 'static,
		F: FnOnce() -> R + Send + 'static,
		R: IntoFuture<Item=(), Error=()> + Send + 'static,
		R::Future: Send + 'static,
	{
		match self.inner {
			Mode::Tokio(ref remote) => {
				remote.spawn(timeout(f, duration, on_timeout))
			},
			Mode::Sync => {
				let _ = timeout(f, duration, on_timeout).wait();
			},
			Mode::ThreadPerFuture => {
				thread::spawn(move || {
					let _ = timeout(f, duration, on_timeout).wait();
				});
			},
		}
	}
}

/// A handle to running event loop. Dropping the handle will cause event loop to finish.
pub struct EventLoopHandle {
	close: Option<futures::Complete<()>>,
	handle: Option<thread::JoinHandle<()>>
}

impl From<EventLoop> for EventLoopHandle {
	fn from(el: EventLoop) -> Self {
		el.handle
	}
}

impl Drop for EventLoopHandle {
	fn drop(&mut self) {
		self.close.take().map(|v| v.send(()));
	}
}

impl EventLoopHandle {
	/// Blocks current thread and waits until the event loop is finished.
	pub fn wait(mut self) -> thread::Result<()> {
		self.handle.take()
			.expect("Handle is taken only in `wait`, `wait` is consuming; qed").join()
	}

	/// Finishes this event loop.
	pub fn close(mut self) {
		let _ = self.close.take()
			.expect("Close is taken only in `close` and `drop`. `close` is consuming; qed")
			.send(());
	}
}
