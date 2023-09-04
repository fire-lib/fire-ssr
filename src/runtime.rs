// Todo check if we should take a snapshot
use crate::{SsrRequest, SsrResponse};

use std::rc::Rc;
use std::pin::Pin;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Serialize, Deserialize};

use deno_core::{
	JsRuntime, RuntimeOptions, op, OpState, Extension, anyhow, ModuleLoader,
	ModuleSpecifier, ModuleSourceFuture, FastString, ResolutionKind, Op,
	resolve_import, ModuleType, ModuleSource, ExtensionFileSource,
	ExtensionFileSourceCode,
	error::generic_error
};

use tokio::sync::oneshot;
use tokio::fs;

use fire::{FirePit, Request};
use fire::header::{RequestHeader, StatusCode, Method, HeaderValues};
use fire::header::values::HeaderName;

use serde_json::Value;

use tracing::trace;


struct StaticModuleLoader {
	root: PathBuf
}

impl ModuleLoader for StaticModuleLoader {
	fn resolve(
		&self,
		specifier: &str,
		referrer: &str,
		_kind: ResolutionKind
	) -> Result<ModuleSpecifier, anyhow::Error> {
		trace!("resolve specifier {specifier:?} with referrer {referrer:?}");
		Ok(resolve_import(specifier, referrer)?)
	}

	fn load(
		&self,
		specifier: &ModuleSpecifier,
		_maybe_referrer: Option<&ModuleSpecifier>,
		_is_dyn_import: bool
	) -> Pin<Box<ModuleSourceFuture>> {
		let specifier = specifier.clone();

		let path = specifier.to_file_path()
			.map(|p| {
				// because this was parsed from a &str it will always be valid
				// utf8
				// let's remove the trailing slash
				let path = p.to_str().and_then(|s| s.get(1..)).unwrap_or("");
				self.root.join(path)
			})
			.map_err(|_| {
				generic_error(format!(
					"Provided module specifier \"{specifier}\" is not a file \
					URL."
				))
			});

		Box::pin(async move {
			let path = path?;

			trace!("reading file {path:?}");

			let is_json = path.extension()
				.and_then(|p| p.to_str())
				.map(|ext| ext == "json")
				.unwrap_or(false);

			let module_type = is_json.then(|| ModuleType::Json)
				.unwrap_or(ModuleType::JavaScript);

			let code = fs::read_to_string(path).await?;

			Ok(ModuleSource::new(module_type, code.into(), &specifier))
		})
	}
}


pub struct Runtime {
	runtime: JsRuntime
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Options(Value);

struct TimersAllowed;

impl deno_web::TimersPermission for TimersAllowed {
	fn allow_hrtime(&mut self) -> bool {
		true
	}

	fn check_unstable(&self, _state: &OpState, _api_name: &'static str) {}
}

impl Runtime {
	pub(crate) async fn new(
		base_dir: PathBuf,
		pit: Option<FirePit>,
		rx: RequestReceiver,
		opts: Value
	) -> Self {
		let ext = Extension {
			name: "fire-ssr",
			deps: &["deno_webidl", "deno_url", "deno_web", "deno_crypto"],
			ops: Cow::Borrowed(&[
				op_tracing_trace::DECL,
				op_tracing_debug::DECL,
				op_tracing_info::DECL,
				op_tracing_warn::DECL,
				op_tracing_error::DECL,

				op_get_options::DECL,
				op_next_request::DECL,
				op_send_response::DECL,
				op_fetch::DECL
			]),
			esm_files: Cow::Borrowed(&[
				ExtensionFileSource {
					specifier: "ext:__fire_ssr_ext/entry.js",
					code: ExtensionFileSourceCode::IncludedInBinary(
						include_str!("./ext_entry.js")
					)
				}
			]),
			esm_entry_point: Some("ext:__fire_ssr_ext/entry.js"),
			..Default::default()
		};

		let mut runtime = JsRuntime::new(RuntimeOptions {
			extensions: vec![
				deno_webidl::deno_webidl::init_ops_and_esm(),
				deno_url::deno_url::init_ops_and_esm(),
				deno_console::deno_console::init_ops_and_esm(),
				deno_web::deno_web::init_ops_and_esm::<TimersAllowed>(
					std::sync::Arc::new(deno_web::BlobStore::default()),
					None
				),
				deno_crypto::deno_crypto::init_ops_and_esm(None),
				ext
			],
			module_loader: Some(Rc::new(StaticModuleLoader {
				root: base_dir
			})),
			..Default::default()
		});

		{
			let state = runtime.op_state();
			let mut state = state.borrow_mut();
			state.put(rx);
			if let Some(pit) = pit {
				state.put(pit);
			}
			state.put(IdCounter::new());
			state.put(ResponseSenders::new());
			state.put(Options(opts));
		}

		let mod_id = runtime.load_main_module(
			&"file:///__fire_ssr_entry.js".parse().unwrap(),
			Some(FastString::from_static(include_str!("./main.js")))
		).await
			.unwrap();
		let res = runtime.mod_evaluate(mod_id);
		runtime.run_event_loop(false).await.unwrap();
		tokio::spawn(async move {
			res.await
				.expect("main.js failed")
				.expect("main.js failed");
		});

		// runtime ready
		Self { runtime }
	}

	/// ## Panics
	/// if the runtime failes.
	pub async fn run(&mut self) {
		self.runtime.run_event_loop(false).await.unwrap();
	}

	pub fn remove_request_receiver(&mut self) {
		self.runtime.op_state().borrow_mut().take::<RequestReceiver>();
	}
}

// registering ops
#[derive(Debug, Clone)]
pub(crate) struct RequestReceiver(
	pub flume::Receiver<(SsrRequest, oneshot::Sender<SsrResponse>)>
);

struct ResponseSenders {
	inner: HashMap<usize, oneshot::Sender<SsrResponse>>
}

impl ResponseSenders {
	pub fn new() -> Self {
		Self {
			inner: HashMap::new()
		}
	}

	pub fn insert(&mut self, id: usize, sender: oneshot::Sender<SsrResponse>) {
		self.inner.insert(id, sender);
	}

	pub fn take(&mut self, id: usize) -> Option<oneshot::Sender<SsrResponse>> {
		self.inner.remove(&id)
	}
}

struct IdCounter {
	inner: usize
}

impl IdCounter {
	pub fn new() -> Self {
		Self {
			inner: 0
		}
	}

	pub fn next(&mut self) -> usize {
		let id = self.inner;
		self.inner = self.inner.wrapping_add(1);
		id
	}
}


#[op]
fn op_tracing_trace(msg: String) {
	tracing::trace!("js: {msg}");
}

#[op]
fn op_tracing_debug(msg: String) {
	tracing::debug!("js: {msg}");
}

#[op]
fn op_tracing_info(msg: String) {
	tracing::info!("js: {msg}");
}

#[op]
fn op_tracing_warn(msg: String) {
	tracing::warn!("js: {msg}");
}

#[op]
fn op_tracing_error(msg: String) {
	tracing::error!("js: {msg}");
}

#[op]
fn op_get_options(state: Rc<RefCell<OpState>>) -> Value {
	state.borrow().borrow::<Options>().clone().0
}


#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpSsrRequest {
	pub id: usize,
	pub req: SsrRequest
}

/// if None get's returned this means you should stop the request loop
#[op]
async fn op_next_request(
	state: Rc<RefCell<OpState>>
) -> Option<OpSsrRequest> {
	let (id, rx) = {
		let mut state = state.borrow_mut();
		// try_borrow since if we should close the connection we remove the
		// RequestReceiver
		let recv = state.try_borrow::<RequestReceiver>()?.clone();
		let id = state.borrow_mut::<IdCounter>().next();

		(id, recv)
	};
	let (req, tx) = rx.0.recv_async().await.ok()?;

	trace!("op next request {id}");

	state.borrow_mut()
		.borrow_mut::<ResponseSenders>()
		.insert(id, tx);

	Some(OpSsrRequest { id, req })
}

#[op]
fn op_send_response(
	state: Rc<RefCell<OpState>>,
	id: usize,
	resp: SsrResponse
) -> Option<()> {
	trace!("op send response {id}");
	let mut state = state.borrow_mut();
	let tx = state.borrow_mut::<ResponseSenders>().take(id)?;
	tx.send(resp).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FetchRequest {
	url: String,
	method: String,
	headers: HashMap<String, String>,
	body: String
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FetchResponse {
	status: u16,
	headers: HashMap<String, String>,
	body: String
}


/// This will not check for a length and might use abitrary amounts of memory
/// todo fix it
#[op]
async fn op_fetch(
	state: Rc<RefCell<OpState>>,
	req: FetchRequest
) -> Result<FetchResponse, anyhow::Error> {
	let mut values = HeaderValues::new();
	for (key, val) in req.headers {
		let key: HeaderName = key.parse().unwrap();
		let _ = values.try_insert(key, val);
	}

	let method: Method = req.method.to_uppercase().parse()
		.unwrap_or(Method::GET);

	if !req.url.starts_with("/") {
		let resp = reqwest::Client::new()
			.request(method, reqwest::Url::parse(&req.url)?)
			.headers(values.into_inner())
			.body(req.body)
			.send().await?;

		let status = resp.status().as_u16();
		let headers: HashMap<_, _> = resp.headers().iter()
			.filter_map(|(key, val)| {
				val.to_str().ok().map(|s| (key.to_string(), s.to_string()))
			})
			.collect();

		return Ok(FetchResponse {
			status, headers,
			body: resp.text().await?
		})
	}

	let header = RequestHeader {
		address: "0.0.0.0:0".parse().unwrap(),
		method,
		uri: req.url.parse()?,
		values
	};

	let mut req = Request::new(header, req.body.into());

	let pit = {
		let state = state.borrow();
		state.try_borrow::<FirePit>().cloned()
	};

	let res = match pit {
		Some(pit) => match pit.route(&mut req).await {
			Some(res) => res?,
			None => StatusCode::NOT_FOUND.into()
		},
		None => StatusCode::NOT_FOUND.into()
	};

	let mut headers: HashMap<_, _> = res.header.values.into_inner().iter()
		.filter_map(|(key, val)| {
			val.to_str().ok().map(|s| (key.to_string(), s.to_string()))
		})
		.collect();

	headers.insert("content-type".into(), res.header.content_type.to_string());

	Ok(FetchResponse {
		status: res.header.status_code.as_u16(),
		headers,
		body: res.body.into_string().await
			.unwrap_or_else(|_| "".into())
	})
}