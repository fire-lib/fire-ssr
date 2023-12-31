use crate::{SsrRequest, SsrResponse};
use crate::runtime::{Runtime, RequestReceiver};

use std::thread;
use std::time::Instant;
use std::path::PathBuf;

use tokio::sync::{oneshot, mpsc};
use tokio::time::{self, Duration};
use tokio::runtime::Handle;

use fire::FirePit;

use serde_json::Value;

use tracing::info;


const GC_INTERVAL: Duration = Duration::from_secs(60);

enum PoolMsg {
	SetPit(FirePit),
	Request((SsrRequest, oneshot::Sender<SsrResponse>))
}

#[derive(Clone)]
pub(crate) struct PoolHandle {
	sender: mpsc::Sender<PoolMsg>
}

impl PoolHandle {
	pub fn new(base_dir: PathBuf, max_threads: usize, opts: Value) -> Self {
		let (sender, rx) = mpsc::channel(max_threads);

		tokio::spawn(async move {
			pool_handler(rx, base_dir, max_threads, opts).await;
		});

		Self { sender }
	}

	pub async fn send_pit(&self, pit: FirePit) {
		self.sender.send(PoolMsg::SetPit(pit)).await
			.map_err(|_| "ssr handler panicked")
			.unwrap();
	}

	pub async fn send_req(
		&self,
		req: SsrRequest
	) -> Option<SsrResponse> {
		let (tx, rx) = oneshot::channel();
		let log = format!("{} {}", req.method, req.uri);
		let start = Instant::now();
		self.sender.send(PoolMsg::Request((req, tx))).await
			.map_err(|_| "ssr handler panicked")
			.unwrap();

		let res = rx.await.ok();
		info!("ssr request to {log} took {}ms", start.elapsed().as_millis());
		res
	}
}

async fn pool_handler(
	mut rx: mpsc::Receiver<PoolMsg>,
	base_dir: PathBuf,
	max_threads: usize,
	opts: Value
) {
	// lets create a pool
	let mut pool = Pool { threads: vec![], max_threads };
	let mut gc_interval = time::interval(GC_INTERVAL);

	let (tx, req_rx) = flume::bounded(max_threads);

	let mut pit = None;

	loop {

		tokio::select! {
			// don't 
			msg = rx.recv() => {
				let msg = match msg.unwrap() {
					PoolMsg::SetPit(fire_pit) => {
						pit = Some(fire_pit);
						continue
					},
					PoolMsg::Request(r) => r
				};

				pool.check_threads();

				if pool.is_empty() || (tx.is_full() && pool.has_capacity()) {
					info!("creating new js runtime");
					// let's create a new js runtime
					pool.threads.push(ThreadHandle::spawn_new_runtime(
						base_dir.clone(),
						pit.clone(),
						RequestReceiver(req_rx.clone()),
						opts.clone()
					));
				}

				tx.send_async(msg).await.unwrap();
			},
			_gc = gc_interval.tick() => {
				pool.check_threads();

				if tx.is_empty() {
					pool.reduce_one();
				}
			}
		}

	}


}

// spawn one
struct Pool {
	threads: Vec<ThreadHandle>,
	max_threads: usize
}

impl Pool {
	fn check_threads(&mut self) {
		let mut ids = vec![];
		for (idx, thread) in self.threads.iter().enumerate() {
			if thread.inner.is_finished() {
				ids.push(idx);
			}
		}

		for id in ids.iter().rev() {
			self.threads.swap_remove(*id);
		}
	}

	fn has_capacity(&self) -> bool {
		self.threads.len() < self.max_threads
	}

	fn is_empty(&self) -> bool {
		self.threads.is_empty()
	}

	fn reduce_one(&mut self) {
		if self.threads.len() <= 1 {
			return
		}

		// check that no thread is shutting down
		let is_shutting_down = self.threads.iter()
			.any(|t| t.shutdown.is_none());
		if is_shutting_down {
			return
		}

		self.threads.last_mut().unwrap().request_shutdown();
	}
}

// pool handler task
// receives request
// - check if some threads thread failed
// - checks if there is a backlog
// - if yes spawn a new thread if possible
// - else 
// receives gc wakeup
// - check if there are still as many threads required
// - else drop some

struct ThreadHandle {
	inner: thread::JoinHandle<()>,
	shutdown: Option<oneshot::Sender<()>>
}

impl ThreadHandle {
	pub fn spawn_new_runtime(
		base_dir: PathBuf,
		pit: Option<FirePit>,
		req_recv: RequestReceiver,
		opts: Value
	) -> Self {
		let tokio_rt = Handle::current();

		let (tx, mut rx) = oneshot::channel();
		let shutdown = Some(tx);

		let inner = thread::spawn(move || {
			tokio_rt.block_on(async move {
				let mut js_rt = Runtime::new(
					base_dir, pit, req_recv, opts
				).await;
				let mut shutdown_received = false;

				loop {
					tokio::select! {
						_ = js_rt.run() => {},
						_shutdown = &mut rx, if !shutdown_received => {
							shutdown_received = true;
							js_rt.remove_request_receiver();
						}
					}
				}
			});
		});

		Self { inner, shutdown }
	}

	pub fn request_shutdown(&mut self) {
		if let Some(tx) = self.shutdown.take() {
			let _: Result<_, _> = tx.send(());
		}
	}
}