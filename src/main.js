import { render } from './server.js';

globalThis.tracing = {
	trace: Deno.core.ops.op_tracing_trace,
	debug: Deno.core.ops.op_tracing_debug,
	info: Deno.core.ops.op_tracing_info,
	warn: Deno.core.ops.op_tracing_warn,
	error: Deno.core.ops.op_tracing_error
};

globalThis.fetch = async (addr, params = {}) => {
	let headers = params.headers ?? {};
	let method = params.method ?? 'GET';
	let body = params.body ?? '';
	method = method.toUpperCase();

	const resp = await Deno.core.ops.op_fetch({
		url: addr,
		method,
		headers,
		body
	});

	return {
		status: resp.status,
		headers: resp.headers,
		json: async () => {
			return JSON.parse(resp.body);
		},
		text: async () => {
			return resp.body;
		}
	};
};

class ConcurrentCounter {
	constructor(limit) {
		this.max = limit;
		this.current = 0;

		this.listener = null;
	}

	/// returns if another up can be called
	async ready() {
		if (this.current < this.max)
			return;

		tracing.warn("Concurrent limit reached");

		return new Promise(resolve => {
			this.listener = resolve;
		});
	}

	up() {
		this.current += 1;
	}

	down() {
		this.current -= 1;
		if (this.listener && this.current < this.max) {
			const trigger = this.listener;
			this.listener = null;
			trigger();
		}
	}
}

const CONCURRENCY_LIMIT = 1000;

async function main() {
	const opts = Deno.core.ops.op_get_options() ?? {};
	const concurrent = new ConcurrentCounter(CONCURRENCY_LIMIT);

	// todo we need to handle multiple request concurrently
	while (true) {
		await concurrent.ready();
		const { id, req } = await Deno.core.opAsync('op_next_request');

		(async () => {
			concurrent.up();
			tracing.trace("received request");

			let resp = {};
			try {
				resp = await render(req, opts);
				if (!resp)
					resp = {};
				if (!resp.status)
					resp.status = 404;
				if (!resp.fields)
					resp.fields = {};
			} catch (e) {
				// todo handle this differently
				tracing.error("render error");
				resp = {
					status: 500,
					fields: {
						head: '',
						body: e.toString()
					}
				};
			}

			Deno.core.ops.op_send_response(id, resp);
			concurrent.down();
		})();
	}
}
main();