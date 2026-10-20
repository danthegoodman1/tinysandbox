#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "quickjs.h"

#define TINYSANDBOX_QUICKJS_STACK_SIZE (768 * 1024)
#define TINYSANDBOX_ABI_VERSION 15

/* Host answer to a call: the response is staged, or it will arrive later. */
#define TB_HOST_INLINE 0
#define TB_HOST_DEFERRED 1
/* call_id values other than a reserved slot: the guest would await the answer
 * but every slot is taken, or the guest needs the answer inline. */
#define TB_CALL_NO_SLOT (-1)
#define TB_CALL_INLINE (-2)
/* tinysandbox_run/tinysandbox_resolve/tinysandbox_wake returned with the guest
 * still alive. */
#define TB_SUSPENDED INT32_MIN
#define TB_MAX_PENDING 64

__attribute__((import_module("tinysandbox"), import_name("host_call")))
int32_t tb_host_call(const uint8_t *op, int32_t op_len, const uint8_t *json, int32_t json_len,
                     int32_t call_id);

__attribute__((import_module("tinysandbox"), import_name("host_response_len")))
int32_t tb_host_response_len(void);

__attribute__((import_module("tinysandbox"), import_name("host_response_read")))
int32_t tb_host_response_read(uint8_t *ptr, int32_t len);

__attribute__((import_module("tinysandbox"), import_name("write_stdout")))
int32_t tb_write_stdout(const uint8_t *ptr, int32_t len);

__attribute__((import_module("tinysandbox"), import_name("write_stderr")))
int32_t tb_write_stderr(const uint8_t *ptr, int32_t len);

__attribute__((import_module("tinysandbox"), import_name("should_interrupt")))
int32_t tb_should_interrupt(void);

static int quickjs_should_interrupt(JSRuntime *rt, void *opaque)
{
    (void)rt;
    (void)opaque;
    return tb_should_interrupt() != 0;
}

typedef struct UnhandledRejection {
    JSValue promise;
    JSValue reason;
    struct UnhandledRejection *next;
} UnhandledRejection;

typedef struct {
    UnhandledRejection *head;
    UnhandledRejection *tail;
} UnhandledRejectionState;

/* One guest program, alive across suspensions.
 *
 * A guest `await` on a host call parks its coroutine on the JS heap, so the
 * runtime outlives the call that started it and the wasm stack unwinds
 * normally. That only works if nothing the runtime needs lives on this
 * module's C stack: QuickJS keeps the address of `rejections`, so a stack
 * local would dangle the moment tinysandbox_run returned. */
typedef struct {
    JSValue resolve;
    JSValue reject;
    int in_use;
} PendingCall;

typedef struct {
    JSRuntime *rt;
    JSContext *ctx;
    UnhandledRejectionState rejections;
    PendingCall pending[TB_MAX_PENDING];
    int pending_count;
    int active;
    /* process.exit() was called: nothing else in the program may run. */
    int exited;
    /* The entry script's completion promise, settled when its top-level code
     * finishes, including any top-level await. */
    JSValue main;
    /* The glue's event-loop hooks: how long the loop may wait, and running the
     * timers and immediates that are due. */
    JSValue timer_delay;
    JSValue timer_begin;
    JSValue timer_run;
    /* Milliseconds from the last suspension until the next timer is due, or -1
     * when the guest waits on host calls alone. */
    int32_t wake_delay;
} Session;

static Session tb_session;

static int32_t session_reserve_slot(void)
{
    for (int32_t i = 0; i < TB_MAX_PENDING; i++) {
        if (!tb_session.pending[i].in_use) {
            return i;
        }
    }
    return TB_CALL_NO_SLOT;
}

static const char TINYSANDBOX_GLUE[] =
"(() => {\n"
"  const hostCall = globalThis.__tinysandbox_host_call\n"
"  const writeStdout = globalThis.__tinysandbox_stdout\n"
"  const writeStderr = globalThis.__tinysandbox_stderr\n"
"  const exit = globalThis.__tinysandbox_exit\n"
"  const evalModule = globalThis.__tinysandbox_eval_module\n"
"  const registerTimers = globalThis.__tinysandbox_timers\n"
"  const config = globalThis.__tinysandboxConfig\n"
"  delete globalThis.__tinysandbox_host_call\n"
"  delete globalThis.__tinysandbox_stdout\n"
"  delete globalThis.__tinysandbox_stderr\n"
"  delete globalThis.__tinysandbox_exit\n"
"  delete globalThis.__tinysandbox_eval_module\n"
"  delete globalThis.__tinysandbox_timers\n"
"  delete globalThis.__tinysandboxConfig\n"
"  function lower(code) { return String(code || '').toLowerCase().replace(/^e/, '') }\n"
"  function normalizeEncoding(encoding) {\n"
"    const enc = encoding === undefined || encoding === null ? 'utf8' : lower(encoding)\n"
"    if (enc === 'utf-8') return 'utf8'\n"
"    return enc\n"
"  }\n"
"  function fsError(e) {\n"
"    if (!e || e.code === undefined) return new Error(e && e.message ? e.message : 'host call failed')\n"
"    const where = e.path !== undefined ? `, ${e.syscall} '${e.path}'` : (e.syscall ? `, ${e.syscall}` : '')\n"
"    const err = new Error(`${e.code}: ${e.message}${where}`)\n"
"    err.code = e.code\n"
"    err.errno = e.errno\n"
"    err.syscall = e.syscall\n"
"    if (e.path !== undefined) err.path = e.path\n"
"    return err\n"
"  }\n"
"  function unwrapFs(response) {\n"
"    if (response && response.error) throw fsError(response.error)\n"
"    return response ? response.value : undefined\n"
"  }\n"
"  function call(op, args) { return unwrapFs(hostCall(op, JSON.stringify(args === undefined ? null : args))) }\n"
"  // The shim holds at most 64 deferred host answers (TB_MAX_PENDING). Globals\n"
"  // past that fail fast; fs.promises calls wait for a slot, as Node queues work\n"
"  // for its thread pool.\n"
"  const MAX_DEFERRED = 64\n"
"  let deferredCount = 0\n"
"  const slotWaiters = []\n"
"  function holdSlot(promise) {\n"
"    deferredCount++\n"
"    const release = () => {\n"
"      deferredCount--\n"
"      const next = slotWaiters.shift()\n"
"      if (next) next()\n"
"    }\n"
"    promise.then(release, release)\n"
"    return promise\n"
"  }\n"
"  // Passing true lets the host defer its answer, as an asynchronous filesystem\n"
"  // must. An answer given inline settles on the next microtask.\n"
"  function callAsync(op, args) {\n"
"    if (deferredCount >= MAX_DEFERRED) return new Promise(resolve => slotWaiters.push(resolve)).then(() => callAsync(op, args))\n"
"    const response = hostCall(op, JSON.stringify(args === undefined ? null : args), true)\n"
"    return response instanceof Promise ? holdSlot(response).then(unwrapFs) : unwrapFs(response)\n"
"  }\n"
"  function globalError(e) {\n"
"    const err = new Error(e && e.message ? e.message : 'host global failed')\n"
"    if (e && e.code !== undefined) err.code = e.code\n"
"    return err\n"
"  }\n"
"  function unwrapGlobal(response) {\n"
"    if (response && response.error) throw globalError(response.error)\n"
"    return response ? response.value : undefined\n"
"  }\n"
"  function callGlobal(name, args) {\n"
"    const response = hostCall('global', JSON.stringify({ name, args: args === undefined ? null : args }), true)\n"
"    // Always a promise, so one guest script works on every host. A host that\n"
"    // answered inline settles it immediately: a microtask, not a suspension.\n"
"    if (response instanceof Promise) return holdSlot(response).then(unwrapGlobal)\n"
"    try { return Promise.resolve(unwrapGlobal(response)) } catch (error) { return Promise.reject(error) }\n"
"  }\n"
"  function installGlobals(paths) {\n"
"    // Namespace nodes use a null prototype so names like __proto__ stay\n"
"    // ordinary properties, and only nodes created here may be extended.\n"
"    const created = new Set()\n"
"    for (const path of paths) {\n"
"      const parts = path.split('.')\n"
"      let node = globalThis\n"
"      for (let i = 0; i < parts.length - 1; i++) {\n"
"        let child = node[parts[i]]\n"
"        if (child === undefined) {\n"
"          child = Object.create(null)\n"
"          node[parts[i]] = child\n"
"          created.add(child)\n"
"        } else if (!created.has(child)) {\n"
"          const taken = parts.slice(0, i + 1).join('.')\n"
"          throw new Error(`tinysandbox: global '${path}' collides with existing global '${taken}'`)\n"
"        }\n"
"        node = child\n"
"      }\n"
"      const leaf = parts[parts.length - 1]\n"
"      if (node[leaf] !== undefined) {\n"
"        throw new Error(`tinysandbox: global '${path}' collides with an existing global`)\n"
"      }\n"
"      node[leaf] = arg => callGlobal(path, arg)\n"
"    }\n"
"  }\n"
"  function utf8Encode(value) {\n"
"    const out = []\n"
"    const input = String(value)\n"
"    for (let i = 0; i < input.length; i++) {\n"
"      let code = input.charCodeAt(i)\n"
"      if (code >= 0xd800 && code <= 0xdbff && i + 1 < input.length) {\n"
"        const next = input.charCodeAt(i + 1)\n"
"        if (next >= 0xdc00 && next <= 0xdfff) {\n"
"          code = 0x10000 + ((code - 0xd800) << 10) + (next - 0xdc00)\n"
"          i++\n"
"        }\n"
"      }\n"
"      if (code <= 0x7f) out.push(code)\n"
"      else if (code <= 0x7ff) out.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f))\n"
"      else if (code <= 0xffff) out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f))\n"
"      else out.push(0xf0 | (code >> 18), 0x80 | ((code >> 12) & 0x3f), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f))\n"
"    }\n"
"    return out\n"
"  }\n"
"  function utf8Decode(bytes) {\n"
"    let out = ''\n"
"    for (let i = 0; i < bytes.length;) {\n"
"      const b0 = bytes[i++]\n"
"      if (b0 === undefined) break\n"
"      let code\n"
"      if (b0 < 0x80) code = b0\n"
"      else if ((b0 & 0xe0) === 0xc0) code = ((b0 & 0x1f) << 6) | (bytes[i++] & 0x3f)\n"
"      else if ((b0 & 0xf0) === 0xe0) code = ((b0 & 0x0f) << 12) | ((bytes[i++] & 0x3f) << 6) | (bytes[i++] & 0x3f)\n"
"      else code = ((b0 & 0x07) << 18) | ((bytes[i++] & 0x3f) << 12) | ((bytes[i++] & 0x3f) << 6) | (bytes[i++] & 0x3f)\n"
"      if (code <= 0xffff) out += String.fromCharCode(code)\n"
"      else {\n"
"        code -= 0x10000\n"
"        out += String.fromCharCode(0xd800 + (code >> 10), 0xdc00 + (code & 0x3ff))\n"
"      }\n"
"    }\n"
"    return out\n"
"  }\n"
"  const base64Chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'\n"
"  const base64Codes = (() => {\n"
"    const out = {}\n"
"    for (let i = 0; i < base64Chars.length; i++) out[base64Chars[i]] = i\n"
"    return out\n"
"  })()\n"
"  function base64Encode(bytes) {\n"
"    const chunks = []\n"
"    let chunk = ''\n"
"    let i = 0\n"
"    function emit(value) {\n"
"      chunk += value\n"
"      if (chunk.length >= 16384) { chunks.push(chunk); chunk = '' }\n"
"    }\n"
"    for (; i + 2 < bytes.length; i += 3) {\n"
"      const n = (bytes[i] << 16) | (bytes[i + 1] << 8) | bytes[i + 2]\n"
"      emit(base64Chars[(n >> 18) & 63] + base64Chars[(n >> 12) & 63] + base64Chars[(n >> 6) & 63] + base64Chars[n & 63])\n"
"    }\n"
"    if (i < bytes.length) {\n"
"      const b0 = bytes[i]\n"
"      const b1 = i + 1 < bytes.length ? bytes[i + 1] : 0\n"
"      const n = (b0 << 16) | (b1 << 8)\n"
"      emit(base64Chars[(n >> 18) & 63] + base64Chars[(n >> 12) & 63] + (i + 1 < bytes.length ? base64Chars[(n >> 6) & 63] : '=') + '=')\n"
"    }\n"
"    return chunks.length === 0 ? chunk : chunks.join('') + chunk\n"
"  }\n"
"  function base64Decode(value) {\n"
"    const input = String(value)\n"
"    if (input.length === 0) return new Uint8Array()\n"
"    if (input.length % 4 !== 0) throw new TypeError('Invalid base64 string')\n"
"    let padding = 0\n"
"    if (input[input.length - 1] === '=') padding++\n"
"    if (input[input.length - 2] === '=') padding++\n"
"    const out = new Uint8Array(input.length / 4 * 3 - padding)\n"
"    let outIndex = 0\n"
"    for (let i = 0; i < input.length; i += 4) {\n"
"      const c0 = base64Codes[input[i]]\n"
"      const c1 = base64Codes[input[i + 1]]\n"
"      const c2 = input[i + 2] === '=' ? 64 : base64Codes[input[i + 2]]\n"
"      const c3 = input[i + 3] === '=' ? 64 : base64Codes[input[i + 3]]\n"
"      if (c0 === undefined || c1 === undefined || c2 === undefined || c3 === undefined) throw new TypeError('Invalid base64 string')\n"
"      const n = (c0 << 18) | (c1 << 12) | ((c2 & 63) << 6) | (c3 & 63)\n"
"      out[outIndex++] = (n >> 16) & 255\n"
"      if (c2 !== 64) out[outIndex++] = (n >> 8) & 255\n"
"      if (c3 !== 64) out[outIndex++] = n & 255\n"
"    }\n"
"    return out\n"
"  }\n"
"  function hexEncode(bytes) {\n"
"    let out = ''\n"
"    for (const byte of bytes) out += (byte >>> 4).toString(16) + (byte & 15).toString(16)\n"
"    return out\n"
"  }\n"
"  function hexDecode(value) {\n"
"    const input = String(value)\n"
"    if (input.length % 2 !== 0) throw new TypeError('Invalid hex string')\n"
"    const out = []\n"
"    for (let i = 0; i < input.length; i += 2) {\n"
"      const byte = Number.parseInt(input.slice(i, i + 2), 16)\n"
"      if (!Number.isFinite(byte)) throw new TypeError('Invalid hex string')\n"
"      out.push(byte)\n"
"    }\n"
"    return out\n"
"  }\n"
"  function bytesFrom(value, encoding) {\n"
"    if (value instanceof Uint8Array) return value\n"
"    if (value instanceof ArrayBuffer) return new Uint8Array(value)\n"
"    if (Array.isArray(value)) return value.map(v => v & 255)\n"
"    const enc = normalizeEncoding(encoding)\n"
"    if (enc === 'base64') return base64Decode(value)\n"
"    if (enc === 'hex') return hexDecode(value)\n"
"    if (enc !== 'utf8') throw new TypeError(`Unsupported encoding: ${encoding}`)\n"
"    return utf8Encode(value)\n"
"  }\n"
"  function headerName(name) { return String(name).toLowerCase() }\n"
"  function headerValue(value) { return String(value).trim() }\n"
"  class Headers {\n"
"    constructor(init) {\n"
"      this._values = Object.create(null)\n"
"      this._names = []\n"
"      if (init === undefined || init === null) return\n"
"      if (init instanceof Headers) {\n"
"        init.forEach((value, name) => this.append(name, value))\n"
"      } else if (Array.isArray(init)) {\n"
"        for (const pair of init) {\n"
"          if (!pair || pair.length < 2) throw new TypeError('Headers constructor: expected name/value pairs')\n"
"          this.append(pair[0], pair[1])\n"
"        }\n"
"      } else if (typeof init === 'object') {\n"
"        for (const name of Object.keys(init)) this.append(name, init[name])\n"
"      } else {\n"
"        throw new TypeError('Headers constructor: unsupported initializer')\n"
"      }\n"
"    }\n"
"    append(name, value) {\n"
"      const key = headerName(name)\n"
"      const text = headerValue(value)\n"
"      if (Object.prototype.hasOwnProperty.call(this._values, key)) this._values[key] += `, ${text}`\n"
"      else { this._values[key] = text; this._names.push(key); this._names.sort() }\n"
"    }\n"
"    get(name) {\n"
"      const key = headerName(name)\n"
"      return Object.prototype.hasOwnProperty.call(this._values, key) ? this._values[key] : null\n"
"    }\n"
"    has(name) { return Object.prototype.hasOwnProperty.call(this._values, headerName(name)) }\n"
"    forEach(callback, thisArg) {\n"
"      for (const name of this._names) callback.call(thisArg, this._values[name], name, this)\n"
"    }\n"
"    entries() { return this._names.map(name => [name, this._values[name]])[Symbol.iterator]() }\n"
"    [Symbol.iterator]() { return this.entries() }\n"
"  }\n"
"  function headersToPairs(headers) {\n"
"    const out = []\n"
"    headers.forEach((value, name) => out.push([name, value]))\n"
"    return out\n"
"  }\n"
"  function normalizeFetchBody(body) {\n"
"    if (body === undefined || body === null) return null\n"
"    if (typeof body === 'string') return bytesFrom(body, 'utf8')\n"
"    if (body instanceof Uint8Array || body instanceof ArrayBuffer) return bytesFrom(body)\n"
"    throw new TypeError('fetch body must be a string, Uint8Array, or ArrayBuffer in tinysandbox')\n"
"  }\n"
"  function normalizeFetchRequest(input, init) {\n"
"    init = init === undefined || init === null ? {} : Object(init)\n"
"    if (init.signal !== undefined && init.signal !== null) throw new TypeError('AbortSignal is not supported in tinysandbox fetch')\n"
"    const method = init.method === undefined ? 'GET' : String(init.method).toUpperCase()\n"
"    const body = normalizeFetchBody(init.body)\n"
"    if ((method === 'GET' || method === 'HEAD') && body !== null) throw new TypeError('Request with GET/HEAD method cannot have body.')\n"
"    return { url: String(input), method, headers: headersToPairs(new Headers(init.headers)), body: body === null ? null : base64Encode(body) }\n"
"  }\n"
"  function fetchFailed(error) {\n"
"    const err = new TypeError('fetch failed')\n"
"    const cause = new Error(error && error.message ? error.message : 'fetch failed')\n"
"    if (error && error.code !== undefined) cause.code = error.code\n"
"    err.cause = cause\n"
"    return err\n"
"  }\n"
"  function callFetch(request) {\n"
"    const response = hostCall('fetch', JSON.stringify(request))\n"
"    if (response && response.error) throw fetchFailed(response.error)\n"
"    return response ? response.value : undefined\n"
"  }\n"
"  function reasonPhrase(status) {\n"
"    switch (Number(status)) {\n"
"      case 100: return 'Continue'\n"
"      case 101: return 'Switching Protocols'\n"
"      case 102: return 'Processing'\n"
"      case 200: return 'OK'\n"
"      case 201: return 'Created'\n"
"      case 202: return 'Accepted'\n"
"      case 203: return 'Non-Authoritative Information'\n"
"      case 204: return 'No Content'\n"
"      case 205: return 'Reset Content'\n"
"      case 206: return 'Partial Content'\n"
"      case 300: return 'Multiple Choices'\n"
"      case 301: return 'Moved Permanently'\n"
"      case 302: return 'Found'\n"
"      case 303: return 'See Other'\n"
"      case 304: return 'Not Modified'\n"
"      case 307: return 'Temporary Redirect'\n"
"      case 308: return 'Permanent Redirect'\n"
"      case 400: return 'Bad Request'\n"
"      case 401: return 'Unauthorized'\n"
"      case 403: return 'Forbidden'\n"
"      case 404: return 'Not Found'\n"
"      case 405: return 'Method Not Allowed'\n"
"      case 408: return 'Request Timeout'\n"
"      case 409: return 'Conflict'\n"
"      case 410: return 'Gone'\n"
"      case 418: return \"I'm a Teapot\"\n"
"      case 429: return 'Too Many Requests'\n"
"      case 500: return 'Internal Server Error'\n"
"      case 501: return 'Not Implemented'\n"
"      case 502: return 'Bad Gateway'\n"
"      case 503: return 'Service Unavailable'\n"
"      case 504: return 'Gateway Timeout'\n"
"      default: return ''\n"
"    }\n"
"  }\n"
"  function bodyUnusableError() { return new TypeError('Body is unusable: Body has already been read') }\n"
"  function bytesToArrayBuffer(bytes) {\n"
"    const out = new ArrayBuffer(bytes.length)\n"
"    new Uint8Array(out).set(bytes)\n"
"    return out\n"
"  }\n"
"  class Response {\n"
"    constructor(body, init) {\n"
"      init = init || {}\n"
"      const status = init.status === undefined ? 200 : Number(init.status)\n"
"      this.status = status\n"
"      this.statusText = init.statusText === undefined ? reasonPhrase(status) : String(init.statusText)\n"
"      this.ok = status >= 200 && status <= 299\n"
"      this.url = init.url === undefined ? '' : String(init.url)\n"
"      this.headers = new Headers(init.headers)\n"
"      this.redirected = false\n"
"      this._body = normalizeFetchBody(body) || new Uint8Array()\n"
"      this._bodyUsed = false\n"
"    }\n"
"    get bodyUsed() { return this._bodyUsed }\n"
"    _consume() {\n"
"      if (this._bodyUsed) throw bodyUnusableError()\n"
"      this._bodyUsed = true\n"
"      return this._body\n"
"    }\n"
"    text() {\n"
"      try { return Promise.resolve(utf8Decode(this._consume())) }\n"
"      catch (err) { return Promise.reject(err) }\n"
"    }\n"
"    json() {\n"
"      try { return Promise.resolve(JSON.parse(utf8Decode(this._consume()))) }\n"
"      catch (err) { return Promise.reject(err) }\n"
"    }\n"
"    arrayBuffer() {\n"
"      try { return Promise.resolve(bytesToArrayBuffer(this._consume())) }\n"
"      catch (err) { return Promise.reject(err) }\n"
"    }\n"
"  }\n"
"  function fetch(input, init) {\n"
"    try {\n"
"      const request = normalizeFetchRequest(input, init)\n"
"      const value = callFetch(request)\n"
"      return Promise.resolve(new Response(base64Decode(value.body || ''), { status: value.status, headers: value.headers, url: request.url }))\n"
"    } catch (err) {\n"
"      return Promise.reject(err)\n"
"    }\n"
"  }\n"
"  function encodingFrom(options) {\n"
"    if (typeof options === 'string') return normalizeEncoding(options)\n"
"    if (options && typeof options.encoding === 'string') return normalizeEncoding(options.encoding)\n"
"    return null\n"
"  }\n"
"  function pathValue(path) { return String(path) }\n"
"  function recursive(options) { return !!(options && typeof options === 'object' && options.recursive) }\n"
"  function force(options) { return !!(options && typeof options === 'object' && options.force) }\n"
"  function withFileTypes(options) { return !!(options && typeof options === 'object' && options.withFileTypes) }\n"
"  function bufferFromBytes(bytes) {\n"
"    const buffer = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes)\n"
"    Object.setPrototypeOf(buffer, Buffer.prototype)\n"
"    return buffer\n"
"  }\n"
"  class Buffer extends Uint8Array {\n"
"    static from(value, encoding) { return bufferFromBytes(bytesFrom(value, encoding)) }\n"
"    static alloc(size) { return bufferFromBytes(new Uint8Array(Math.max(0, Number(size) || 0))) }\n"
"    static isBuffer(value) { return value instanceof Buffer }\n"
"    toString(encoding) {\n"
"      const enc = normalizeEncoding(encoding)\n"
"      if (enc === 'utf8') return utf8Decode(this)\n"
"      if (enc === 'base64') return base64Encode(this)\n"
"      if (enc === 'hex') return hexEncode(this)\n"
"      throw new TypeError(`Unsupported encoding: ${encoding}`)\n"
"    }\n"
"  }\n"
"  class Stats {\n"
"    constructor(value) { this.size = value.size; this.mode = value.isDirectory ? 0o40755 : 0o100644 }\n"
"    isFile() { return (this.mode & 0o170000) === 0o100000 }\n"
"    isDirectory() { return (this.mode & 0o170000) === 0o40000 }\n"
"  }\n"
"  class Dirent {\n"
"    constructor(value) { this.name = value.name; this._isFile = !!value.isFile; this._isDirectory = !!value.isDirectory }\n"
"    isFile() { return this._isFile }\n"
"    isDirectory() { return this._isDirectory }\n"
"  }\n"
"  function fileContents(data, options) {\n"
"    const enc = encodingFrom(options)\n"
"    const buffer = Buffer.from(base64Decode(data))\n"
"    return enc === 'utf8' ? buffer.toString(enc) : buffer\n"
"  }\n"
"  function fileData(data, options) { return base64Encode(bytesFrom(data, options && options.encoding)) }\n"
"  // Each operation is written once as a generator that yields its host request.\n"
"  // The sync API answers it inline; fs.promises awaits an answer the host may\n"
"  // defer to an asynchronous filesystem.\n"
"  const operations = {\n"
"    readFile: function* (path, options) { return fileContents(yield ['readFile', { path: pathValue(path) }], options) },\n"
"    writeFile: function* (path, data, options) { yield ['writeFile', { path: pathValue(path), data: fileData(data, options) }] },\n"
"    appendFile: function* (path, data, options) { yield ['appendFile', { path: pathValue(path), data: fileData(data, options) }] },\n"
"    mkdir: function* (path, options) { return yield ['mkdir', { path: pathValue(path), recursive: recursive(options) }] },\n"
"    readdir: function* (path, options) {\n"
"      const entries = yield ['readdir', { path: pathValue(path), withFileTypes: withFileTypes(options) }]\n"
"      return withFileTypes(options) ? entries.map(entry => new Dirent(entry)) : entries\n"
"    },\n"
"    stat: function* (path) { return new Stats(yield ['stat', { path: pathValue(path) }]) },\n"
"    rename: function* (oldPath, newPath) { yield ['rename', { from: pathValue(oldPath), to: pathValue(newPath) }] },\n"
"    rm: function* (path, options) { yield ['rm', { path: pathValue(path), recursive: recursive(options), force: force(options) }] },\n"
"    unlink: function* (path) { yield ['unlink', { path: pathValue(path) }] },\n"
"    rmdir: function* (path, options) { yield ['rmdir', { path: pathValue(path), recursive: recursive(options) }] },\n"
"    exists: function* (path) { return !!(yield ['exists', { path: pathValue(path) }]) },\n"
"    open: function* (path, flags) { return yield ['open', { path: pathValue(path), flags: flags === undefined ? 'r' : String(flags) }] },\n"
"    read: function* (fd, buffer, offset, length, position) {\n"
"      offset = offset === undefined ? 0 : Number(offset)\n"
"      length = length === undefined ? buffer.length - offset : Number(length)\n"
"      const result = yield ['read', { fd, offset, length, position: position === null || position === undefined ? null : position }]\n"
"      const data = base64Decode(result.data)\n"
"      for (let i = 0; i < data.length; i++) buffer[offset + i] = data[i]\n"
"      return result.bytesRead\n"
"    },\n"
"    write: function* (fd, buffer, offset, length, position) {\n"
"      let data\n"
"      let writePosition\n"
"      if (typeof buffer === 'string') {\n"
"        data = bytesFrom(buffer, length)\n"
"        writePosition = offset === null || offset === undefined ? null : offset\n"
"      } else {\n"
"        offset = offset === undefined ? 0 : Number(offset)\n"
"        length = length === undefined ? buffer.length - offset : Number(length)\n"
"        data = typeof buffer.subarray === 'function' ? buffer.subarray(offset, offset + length) : Array.from(buffer).slice(offset, offset + length)\n"
"        writePosition = position === null || position === undefined ? null : position\n"
"      }\n"
"      return yield ['write', { fd, data: base64Encode(data), position: writePosition }]\n"
"    },\n"
"    ftruncate: function* (fd, len) { yield ['ftruncate', { fd, len: len === undefined ? 0 : len }] },\n"
"    close: function* (fd) { yield ['close', { fd }] },\n"
"    copyFile: function* (src, dest) { yield ['copyFile', { src: pathValue(src), dest: pathValue(dest) }] },\n"
"  }\n"
"  function syncFs(operation) {\n"
"    return function (...args) {\n"
"      const steps = operation(...args)\n"
"      let step = steps.next()\n"
"      while (!step.done) step = steps.next(call(step.value[0], step.value[1]))\n"
"      return step.value\n"
"    }\n"
"  }\n"
"  function promiseFs(operation) {\n"
"    return async function (...args) {\n"
"      const steps = operation(...args)\n"
"      let step = steps.next()\n"
"      while (!step.done) step = steps.next(await callAsync(step.value[0], step.value[1]))\n"
"      return step.value\n"
"    }\n"
"  }\n"
"  const openSync = syncFs(operations.open)\n"
"  const readSync = syncFs(operations.read)\n"
"  const closeSync = syncFs(operations.close)\n"
"  function* readLinesSync(path, options) {\n"
"    const enc = encodingFrom(options) || 'utf8'\n"
"    if (enc !== 'utf8') throw new TypeError(`Unsupported encoding: ${enc}`)\n"
"    const fd = openSync(path, 'r')\n"
"    const chunk = Buffer.alloc(64 * 1024)\n"
"    const line = []\n"
"    try {\n"
"      for (;;) {\n"
"        const n = readSync(fd, chunk, 0, chunk.length, null)\n"
"        if (n === 0) break\n"
"        for (let i = 0; i < n; i++) {\n"
"          const byte = chunk[i]\n"
"          if (byte === 10) {\n"
"            const end = line.length > 0 && line[line.length - 1] === 13 ? line.length - 1 : line.length\n"
"            yield utf8Decode(line.slice(0, end))\n"
"            line.length = 0\n"
"          } else {\n"
"            line.push(byte)\n"
"          }\n"
"        }\n"
"      }\n"
"      if (line.length > 0) yield utf8Decode(line)\n"
"    } finally {\n"
"      closeSync(fd)\n"
"    }\n"
"  }\n"
"  const handleOperations = { read: promiseFs(operations.read), write: promiseFs(operations.write), ftruncate: promiseFs(operations.ftruncate), close: promiseFs(operations.close) }\n"
"  class FileHandle {\n"
"    constructor(fd) { this.fd = fd }\n"
"    async read(buffer, offset, length, position) { return { bytesRead: await this._call('read', buffer, offset, length, position), buffer } }\n"
"    async write(buffer, offset, length, position) { return { bytesWritten: await this._call('write', buffer, offset, length, position), buffer } }\n"
"    async truncate(len) { await this._call('ftruncate', len) }\n"
"    // Like Node, closing twice succeeds. The host releases a descriptor even\n"
"    // when its close fails, so the handle is closed either way.\n"
"    async close() {\n"
"      if (this.fd === -1) return\n"
"      const fd = this.fd\n"
"      this.fd = -1\n"
"      await handleOperations.close(fd)\n"
"    }\n"
"    _call(name, ...args) {\n"
"      if (this.fd !== -1) return handleOperations[name](this.fd, ...args)\n"
"      const err = new Error('file closed')\n"
"      err.code = 'EBADF'\n"
"      err.syscall = name\n"
"      return Promise.reject(err)\n"
"    }\n"
"  }\n"
"  function callbackUnsupported(name) {\n"
"    return function() { throw new Error(`fs.${name} callbacks are not supported in tinysandbox; use fs.${name}Sync or fs.promises`) }\n"
"  }\n"
"  const openAsync = promiseFs(operations.open)\n"
"  const promises = { async open(path, flags) { return new FileHandle(await openAsync(path, flags)) } }\n"
"  const fs = { readLinesSync, promises }\n"
"  for (const name of Object.keys(operations)) {\n"
"    fs[`${name}Sync`] = syncFs(operations[name])\n"
"    if (name !== 'exists') fs[name] = callbackUnsupported(name)\n"
"  }\n"
"  for (const name of ['readFile', 'writeFile', 'appendFile', 'mkdir', 'readdir', 'stat', 'rename', 'rm', 'unlink', 'rmdir', 'copyFile']) promises[name] = promiseFs(operations[name])\n"
"  // Timers. The shim runs the event loop: once the job queue drains it asks\n"
"  // timerDelay() how long it may wait, and once that reaches zero it calls\n"
"  // beginTimers() and then runTimer() until that returns false, draining the\n"
"  // job queue after each callback. As in Node, one turn runs the timers due\n"
"  // when it began and then the immediates queued before its immediate phase.\n"
"  const clock = performance.now.bind(performance)\n"
"  const TIMEOUT_MAX = 2 ** 31 - 1\n"
"  const kTimer = Symbol('timer')\n"
"  const kImmediate = Symbol('immediate')\n"
"  // A min-heap of {timer, due, seq}. Clearing or rescheduling a timer leaves\n"
"  // its old entry behind; an entry is live only while its timer points at it.\n"
"  const timerHeap = []\n"
"  const activeTimers = new Map()\n"
"  const immediates = []\n"
"  let immediateHead = 0\n"
"  let immediateBudget = 0\n"
"  let nextTimerId = 1\n"
"  let timerSeq = 0\n"
"  let refedTimers = 0\n"
"  let queuedImmediates = 0\n"
"  let refedImmediates = 0\n"
"  let turnStarted = 0\n"
"  let timersPhase = false\n"
"  function precedes(a, b) { return a.due < b.due || (a.due === b.due && a.seq < b.seq) }\n"
"  function heapPush(entry) {\n"
"    let i = timerHeap.push(entry) - 1\n"
"    while (i > 0) {\n"
"      const parent = (i - 1) >> 1\n"
"      if (!precedes(timerHeap[i], timerHeap[parent])) break\n"
"      ;[timerHeap[i], timerHeap[parent]] = [timerHeap[parent], timerHeap[i]]\n"
"      i = parent\n"
"    }\n"
"  }\n"
"  function heapPop() {\n"
"    const last = timerHeap.pop()\n"
"    if (timerHeap.length === 0) return\n"
"    timerHeap[0] = last\n"
"    let i = 0\n"
"    for (;;) {\n"
"      const left = 2 * i + 1\n"
"      let least = i\n"
"      if (left < timerHeap.length && precedes(timerHeap[left], timerHeap[least])) least = left\n"
"      if (left + 1 < timerHeap.length && precedes(timerHeap[left + 1], timerHeap[least])) least = left + 1\n"
"      if (least === i) return\n"
"      ;[timerHeap[i], timerHeap[least]] = [timerHeap[least], timerHeap[i]]\n"
"      i = least\n"
"    }\n"
"  }\n"
"  function nextTimer() {\n"
"    while (timerHeap.length > 0 && timerHeap[0].timer.entry !== timerHeap[0]) heapPop()\n"
"    return timerHeap[0]\n"
"  }\n"
"  function schedule(timer, from) {\n"
"    if (!activeTimers.has(timer.id)) {\n"
"      activeTimers.set(timer.id, timer)\n"
"      if (timer.ref) refedTimers++\n"
"    }\n"
"    timer.entry = { timer, due: from + timer.delay, seq: timerSeq++ }\n"
"    heapPush(timer.entry)\n"
"    // Drop stale entries once they outnumber live ones. A sorted array is a heap.\n"
"    if (timerHeap.length > 64 && timerHeap.length > 2 * activeTimers.size) {\n"
"      const live = timerHeap.filter(entry => entry.timer.entry === entry).sort((a, b) => precedes(a, b) ? -1 : 1)\n"
"      for (let i = 0; i < live.length; i++) timerHeap[i] = live[i]\n"
"      timerHeap.length = live.length\n"
"    }\n"
"  }\n"
"  function unschedule(timer) {\n"
"    if (!activeTimers.delete(timer.id)) return\n"
"    if (timer.ref) refedTimers--\n"
"    timer.entry = null\n"
"  }\n"
"  function received(value) {\n"
"    if (value === null || value === undefined) return String(value)\n"
"    if (typeof value === 'object') return value.constructor && value.constructor.name ? `an instance of ${value.constructor.name}` : formatInspect(value)\n"
"    return `type ${typeof value} (${inspect(value, true)})`\n"
"  }\n"
"  function timerCallback(callback) {\n"
"    if (typeof callback === 'function') return callback\n"
"    const err = new TypeError(`The \"callback\" argument must be of type function. Received ${received(callback)}`)\n"
"    err.code = 'ERR_INVALID_ARG_TYPE'\n"
"    throw err\n"
"  }\n"
"  class Timeout {\n"
"    constructor(timer) { this[kTimer] = timer }\n"
"    ref() { setTimerRef(this[kTimer], true); return this }\n"
"    unref() { setTimerRef(this[kTimer], false); return this }\n"
"    hasRef() { return this[kTimer].ref }\n"
"    // Restarts the countdown, including for a timer that already fired.\n"
"    refresh() {\n"
"      const timer = this[kTimer]\n"
"      if (!timer.cleared) schedule(timer, clock())\n"
"      return this\n"
"    }\n"
"    close() { clearTimeout(this); return this }\n"
"    [Symbol.toPrimitive]() { return this[kTimer].id }\n"
"  }\n"
"  function setTimerRef(timer, ref) {\n"
"    if (timer.ref !== ref && activeTimers.has(timer.id)) refedTimers += ref ? 1 : -1\n"
"    timer.ref = ref\n"
"  }\n"
"  function startTimer(callback, delay, args, repeat) {\n"
"    // Node's coercion: anything outside 1..TIMEOUT_MAX, NaN included, becomes 1.\n"
"    let ms = delay * 1\n"
"    if (!(ms >= 1 && ms <= TIMEOUT_MAX)) ms = 1\n"
"    const timer = { id: nextTimerId++, callback: timerCallback(callback), args, delay: ms, repeat, ref: true, cleared: false, entry: null, handle: null }\n"
"    timer.handle = new Timeout(timer)\n"
"    schedule(timer, clock())\n"
"    return timer.handle\n"
"  }\n"
"  function clearTimer(handle) {\n"
"    const timer = handle instanceof Timeout ? handle[kTimer]\n"
"      : typeof handle === 'number' || typeof handle === 'string' ? activeTimers.get(Number(handle)) : undefined\n"
"    if (timer === undefined) return\n"
"    timer.cleared = true\n"
"    unschedule(timer)\n"
"  }\n"
"  function setTimeout(callback, delay, ...args) { return startTimer(callback, delay, args, false) }\n"
"  function setInterval(callback, delay, ...args) { return startTimer(callback, delay, args, true) }\n"
"  function clearTimeout(handle) { clearTimer(handle) }\n"
"  function clearInterval(handle) { clearTimer(handle) }\n"
"  class Immediate {\n"
"    constructor(immediate) { this[kImmediate] = immediate }\n"
"    ref() { setImmediateRef(this[kImmediate], true); return this }\n"
"    unref() { setImmediateRef(this[kImmediate], false); return this }\n"
"    hasRef() { return this[kImmediate].ref }\n"
"  }\n"
"  function setImmediateRef(immediate, ref) {\n"
"    if (immediate.ref !== ref && immediate.queued) refedImmediates += ref ? 1 : -1\n"
"    immediate.ref = ref\n"
"  }\n"
"  function dequeueImmediate(immediate) {\n"
"    if (!immediate.queued) return\n"
"    immediate.queued = false\n"
"    queuedImmediates--\n"
"    if (immediate.ref) refedImmediates--\n"
"  }\n"
"  function setImmediate(callback, ...args) {\n"
"    const immediate = { callback: timerCallback(callback), args, ref: true, queued: true, handle: null }\n"
"    immediate.handle = new Immediate(immediate)\n"
"    immediates.push(immediate)\n"
"    queuedImmediates++\n"
"    refedImmediates++\n"
"    return immediate.handle\n"
"  }\n"
"  function clearImmediate(handle) {\n"
"    if (handle instanceof Immediate) dequeueImmediate(handle[kImmediate])\n"
"  }\n"
"  // Milliseconds until the next callback is due, or -1 when nothing keeps the\n"
"  // loop alive. Unreferenced timers still fire while host calls are pending.\n"
"  function timerDelay(hostPending) {\n"
"    if (!hostPending && refedTimers === 0 && refedImmediates === 0) return -1\n"
"    if (queuedImmediates > 0) return 0\n"
"    const entry = nextTimer()\n"
"    return entry === undefined ? -1 : Math.max(0, Math.ceil(entry.due - clock()))\n"
"  }\n"
"  function beginTimers() {\n"
"    immediates.splice(0, immediateHead)\n"
"    immediateHead = 0\n"
"    turnStarted = clock()\n"
"    timersPhase = true\n"
"  }\n"
"  function runTimer() {\n"
"    if (timersPhase) {\n"
"      const entry = nextTimer()\n"
"      if (entry !== undefined && entry.due <= turnStarted) {\n"
"        heapPop()\n"
"        const timer = entry.timer\n"
"        if (timer.repeat) timer.entry = null\n"
"        else unschedule(timer)\n"
"        timer.callback.apply(timer.handle, timer.args)\n"
"        // An interval reschedules unless its callback cleared or refreshed it.\n"
"        if (timer.repeat && timer.entry === null && activeTimers.has(timer.id)) schedule(timer, turnStarted)\n"
"        return true\n"
"      }\n"
"      timersPhase = false\n"
"      immediateBudget = immediates.length - immediateHead\n"
"    }\n"
"    while (immediateBudget > 0) {\n"
"      immediateBudget--\n"
"      const immediate = immediates[immediateHead]\n"
"      immediates[immediateHead++] = undefined\n"
"      if (!immediate.queued) continue\n"
"      dequeueImmediate(immediate)\n"
"      immediate.callback.apply(immediate.handle, immediate.args)\n"
"      return true\n"
"    }\n"
"    return false\n"
"  }\n"
"  registerTimers(timerDelay, beginTimers, runTimer)\n"
"  function promiseTimer(start, options) {\n"
"    return new Promise(resolve => {\n"
"      const handle = start(resolve)\n"
"      if (options && options.ref === false) handle.unref()\n"
"    })\n"
"  }\n"
"  const timersPromises = {\n"
"    setTimeout: (delay, value, options) => promiseTimer(resolve => setTimeout(resolve, delay, value), options),\n"
"    setImmediate: (value, options) => promiseTimer(resolve => setImmediate(resolve, value), options),\n"
"  }\n"
"  const timersModule = { setTimeout, clearTimeout, setInterval, clearInterval, setImmediate, clearImmediate, promises: timersPromises }\n"
"  const moduleCache = Object.create(null)\n"
"  const MAX_REQUIRE_DEPTH = 256\n"
"  let mainModule = null\n"
"  let requireDepth = 0\n"
"  function normalizeAbsolute(path) {\n"
"    const parts = []\n"
"    for (const part of String(path).split('/')) {\n"
"      if (part === '' || part === '.') continue\n"
"      if (part === '..') parts.pop()\n"
"      else parts.push(part)\n"
"    }\n"
"    return parts.length === 0 ? '/' : `/${parts.join('/')}`\n"
"  }\n"
"  function dirname(path) {\n"
"    if (path === '[eval]') return config.cwd\n"
"    const normalized = normalizeAbsolute(path)\n"
"    const index = normalized.lastIndexOf('/')\n"
"    return index <= 0 ? '/' : normalized.slice(0, index)\n"
"  }\n"
"  function joinPath(dir, name) { return normalizeAbsolute(dir === '/' ? `/${name}` : `${dir}/${name}`) }\n"
"  function isPathSpecifier(specifier) { return specifier === '.' || specifier === '..' || specifier.startsWith('/') || specifier.startsWith('./') || specifier.startsWith('../') }\n"
"  function statOrNull(path) {\n"
"    try { return fs.statSync(path) }\n"
"    catch (err) {\n"
"      if (err && (err.code === 'ENOENT' || err.code === 'ENOTDIR')) return null\n"
"      throw err\n"
"    }\n"
"  }\n"
"  function fileCandidate(path) {\n"
"    const stat = statOrNull(path)\n"
"    return stat && stat.isFile() ? normalizeAbsolute(path) : null\n"
"  }\n"
"  function requireStack(parent) {\n"
"    const stack = []\n"
"    for (let module = parent; module; module = module.parent) {\n"
"      if (module.filename !== '[eval]') stack.push(module.filename)\n"
"    }\n"
"    return stack\n"
"  }\n"
"  function moduleNotFound(specifier, parent) {\n"
"    const stack = requireStack(parent)\n"
"    const suffix = stack.length === 0 ? '' : `\\nRequire stack:\\n${stack.map(path => `- ${path}`).join('\\n')}`\n"
"    const err = new Error(`Cannot find module '${specifier}'${suffix}`)\n"
"    err.code = 'MODULE_NOT_FOUND'\n"
"    err.requireStack = stack\n"
"    return err\n"
"  }\n"
"  function bareSpecifierError(specifier, parent) {\n"
"    const err = moduleNotFound(specifier, parent)\n"
"    err.message = `Cannot find module '${specifier}': there is no node_modules in tinysandbox`\n"
"    return err\n"
"  }\n"
"  function resolveModule(specifier, parent) {\n"
"    const request = String(specifier)\n"
"    if (!isPathSpecifier(request)) throw bareSpecifierError(request, parent)\n"
"    const base = request.startsWith('/') ? normalizeAbsolute(request) : joinPath(parent.dirname, request)\n"
"    const directoryOnly = request === '.' || request === '..' || request.endsWith('/')\n"
"    if (!directoryOnly) {\n"
"      const file = fileCandidate(base) || fileCandidate(`${base}.js`) || fileCandidate(`${base}.json`)\n"
"      if (file) return file\n"
"    }\n"
"    const stat = statOrNull(base)\n"
"    if (stat && stat.isDirectory()) {\n"
"      const index = fileCandidate(joinPath(base, 'index.js'))\n"
"      if (index) return index\n"
"    }\n"
"    throw moduleNotFound(request, parent)\n"
"  }\n"
"  function stripBomAndShebang(source) {\n"
"    source = String(source)\n"
"    if (source.charCodeAt(0) === 0xfeff) source = source.slice(1)\n"
"    if (!source.startsWith('#!')) return source\n"
"    const end = source.indexOf('\\n')\n"
"    return end === -1 ? '' : `\\n${source.slice(end + 1)}`\n"
"  }\n"
"  function readModuleText(path) { return utf8Decode(base64Decode(call('readFile', { path }))) }\n"
"  function createModule(path, parent) {\n"
"    const module = { id: path, filename: path, path: dirname(path), dirname: dirname(path), exports: {}, loaded: false, parent: parent || null, children: [] }\n"
"    module.require = makeRequire(module)\n"
"    return module\n"
"  }\n"
"  function makeRequire(module) {\n"
"    function localRequire(specifier) {\n"
"      if (specifier === 'timers') return timersModule\n"
"      if (specifier === 'timers/promises') return timersPromises\n"
"      if (!config.vfs) {\n"
"        const err = new Error('filesystem capability is not available in this runtime')\n"
"        err.code = 'ERR_CAPABILITY_UNAVAILABLE'\n"
"        throw err\n"
"      }\n"
"      if (specifier === 'fs') return fs\n"
"      if (specifier === 'fs/promises') return promises\n"
"      if (typeof specifier !== 'string') throw new TypeError(`The \"id\" argument must be of type string. Received ${typeof specifier}`)\n"
"      return loadModule(resolveModule(specifier, module), module)\n"
"    }\n"
"    Object.defineProperty(localRequire, 'main', { get: () => mainModule === null ? undefined : mainModule })\n"
"    localRequire.cache = moduleCache\n"
"    return localRequire\n"
"  }\n"
"  // The entry script runs as an async function so it may use top-level await;\n"
"  // required modules stay synchronous, as require() returns their exports.\n"
"  function executeJsModule(module, source, thisArg, entry) {\n"
"    const wrapper = `(${entry ? 'async ' : ''}function (exports, require, module, __filename, __dirname) {${stripBomAndShebang(source)}\\n})`\n"
"    const compiled = evalModule(wrapper, module.filename)\n"
"    return compiled.call(thisArg, module.exports, module.require, module, module.filename, module.dirname)\n"
"  }\n"
"  function executeJsonModule(module, source) {\n"
"    try { module.exports = JSON.parse(stripBomAndShebang(source)) }\n"
"    catch (err) { throw new SyntaxError(`${module.filename}: ${err && err.message ? err.message : String(err)}`) }\n"
"  }\n"
"  function loadModule(path, parent) {\n"
"    if (Object.prototype.hasOwnProperty.call(moduleCache, path)) return moduleCache[path].exports\n"
"    if (requireDepth >= MAX_REQUIRE_DEPTH) {\n"
"      const err = new Error(`Maximum CommonJS require depth of ${MAX_REQUIRE_DEPTH} exceeded in tinysandbox`)\n"
"      err.code = 'ERR_REQUIRE_DEPTH'\n"
"      throw err\n"
"    }\n"
"    const module = createModule(path, parent)\n"
"    moduleCache[path] = module\n"
"    if (parent) parent.children.push(module)\n"
"    requireDepth++\n"
"    try {\n"
"      const source = readModuleText(path)\n"
"      if (path.endsWith('.json')) executeJsonModule(module, source)\n"
"      else executeJsModule(module, source, module.exports, false)\n"
"      module.loaded = true\n"
"      return module.exports\n"
"    } catch (err) {\n"
"      delete moduleCache[path]\n"
"      throw err\n"
"    } finally {\n"
"      requireDepth--\n"
"    }\n"
"  }\n"
"  function runMain(source, path) {\n"
"    const filename = path === '[eval]' ? path : normalizeAbsolute(path)\n"
"    const module = createModule(filename, null)\n"
"    const isEval = path === '[eval]'\n"
"    mainModule = isEval ? null : module\n"
"    if (!isEval) module.id = '.'\n"
"    moduleCache[filename] = module\n"
"    globalThis.module = module\n"
"    globalThis.exports = module.exports\n"
"    globalThis.require = module.require\n"
"    globalThis.__filename = module.filename\n"
"    globalThis.__dirname = module.dirname\n"
"    const completion = executeJsModule(module, source, isEval ? globalThis : module.exports, true)\n"
"    module.loaded = true\n"
"    globalThis.exports = module.exports\n"
"    return completion\n"
"  }\n"
"  function quote(value) { return `'${String(value).replace(/\\\\/g, '\\\\\\\\').replace(/'/g, \"\\\\'\")}'` }\n"
"  function inspect(value, nested, depth, seen, state) {\n"
"    depth = depth || 0\n"
"    seen = seen || []\n"
"    if (typeof value === 'string') return nested ? quote(value) : value\n"
"    if (typeof value === 'number') return Object.is(value, -0) ? '-0' : String(value)\n"
"    if (value === null || typeof value !== 'object') return String(value)\n"
"    if (seen.indexOf(value) !== -1) { state.circular = true; return '[Circular *1]' }\n"
"    if (value instanceof Buffer) return `<Buffer ${Array.from(value).map(b => b.toString(16).padStart(2, '0')).join(' ')}>`\n"
"    if (value instanceof Uint8Array) return `Uint8Array(${value.length}) [ ${Array.from(value).join(', ')} ]`\n"
"    if (depth > 2) return Array.isArray(value) ? '[Array]' : '[Object]'\n"
"    seen.push(value)\n"
"    if (Array.isArray(value)) {\n"
"      if (value.length === 0) { seen.pop(); return '[]' }\n"
"      const text = `[ ${value.map(item => inspect(item, true, depth + 1, seen, state)).join(', ')} ]`\n"
"      seen.pop()\n"
"      return text\n"
"    }\n"
"    const keys = Object.keys(value)\n"
"    if (keys.length === 0) { seen.pop(); return '{}' }\n"
"    const text = `{ ${keys.map(k => `${k}: ${inspect(value[k], true, depth + 1, seen, state)}`).join(', ')} }`\n"
"    seen.pop()\n"
"    return text\n"
"  }\n"
"  function formatInspect(value) {\n"
"    const state = { circular: false }\n"
"    const text = inspect(value, false, 0, [], state)\n"
"    return state.circular ? `<ref *1> ${text}` : text\n"
"  }\n"
"  function format(args) {\n"
"    if (args.length === 0) return ''\n"
"    if (typeof args[0] !== 'string') return args.map(value => formatInspect(value)).join(' ')\n"
"    let index = 1\n"
"    const first = String(args[0]).replace(/%[sdifjoO%]/g, token => {\n"
"      if (token === '%%') return '%'\n"
"      if (index >= args.length) return token\n"
"      const value = args[index++]\n"
"      if (token === '%s') return String(value)\n"
"      if (token === '%d') return String(Number(value))\n"
"      if (token === '%i') return String(Number.parseInt(value))\n"
"      if (token === '%f') return String(Number.parseFloat(value))\n"
"      if (token === '%j') {\n"
"        try { return JSON.stringify(value) } catch (_) { return '[Circular]' }\n"
"      }\n"
"      return formatInspect(value)\n"
"    })\n"
"    const rest = args.slice(index).map(value => formatInspect(value))\n"
"    return rest.length === 0 ? first : `${first} ${rest.join(' ')}`\n"
"  }\n"
"  function line(args) { return format(args) + '\\n' }\n"
"  if (config.vfs) globalThis.Buffer = Buffer\n"
"  globalThis.Headers = Headers\n"
"  globalThis.Response = Response\n"
"  globalThis.fetch = fetch\n"
"  globalThis.console = { log: (...args) => writeStdout(line(args)), info: (...args) => writeStdout(line(args)), error: (...args) => writeStderr(line(args)), warn: (...args) => writeStderr(line(args)) }\n"
"  Object.assign(globalThis, { setTimeout, clearTimeout, setInterval, clearInterval, setImmediate, clearImmediate })\n"
"  globalThis.process = { argv: config.argv, env: config.env, cwd: () => config.cwd, exit: code => exit(code === undefined ? 0 : Number(code) || 0) }\n"
"  if (Array.isArray(config.globals) && config.globals.length > 0) installGlobals(config.globals)\n"
"  return runMain\n"
"})()\n";

__attribute__((export_name("tinysandbox_alloc")))
void *tinysandbox_alloc(int32_t len)
{
    size_t size = len > 0 ? (size_t)len : 0;
    uint8_t *ptr = malloc(size + 1);
    if (ptr) {
        ptr[size] = '\0';
    }
    return ptr;
}

__attribute__((export_name("tinysandbox_free")))
void tinysandbox_free(void *ptr)
{
    free(ptr);
}

__attribute__((export_name("tinysandbox_abi_version")))
int32_t tinysandbox_abi_version(void)
{
    return TINYSANDBOX_ABI_VERSION;
}

static JSValue js_host_call(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    (void)this_val;
    if (argc < 2) {
        return JS_ThrowTypeError(ctx, "host call requires operation and JSON arguments");
    }

    size_t op_len = 0;
    size_t json_len = 0;
    const char *op = JS_ToCStringLen(ctx, &op_len, argv[0]);
    const char *json = JS_ToCStringLen(ctx, &json_len, argv[1]);
    if (!op || !json) {
        if (op) {
            JS_FreeCString(ctx, op);
        }
        if (json) {
            JS_FreeCString(ctx, json);
        }
        return JS_EXCEPTION;
    }

    /* Only a caller that awaits the answer gets a slot the host may defer on;
     * the synchronous fs API must be answered before this call returns. */
    int deferrable = argc > 2 && JS_ToBool(ctx, argv[2]);
    int32_t call_id = deferrable ? session_reserve_slot() : TB_CALL_INLINE;
    int32_t answer = tb_host_call((const uint8_t *)op, (int32_t)op_len, (const uint8_t *)json,
                                  (int32_t)json_len, call_id);
    JS_FreeCString(ctx, op);
    JS_FreeCString(ctx, json);

    if (answer == TB_HOST_DEFERRED) {
        if (call_id < 0) {
            return JS_ThrowInternalError(ctx, "tinysandbox host call limit exceeded");
        }
        /* The host settles this later. Hand the guest a promise so its await
         * parks on the JS heap and this call returns, letting the wasm stack
         * unwind back to the host's event loop. */
        JSValue resolving[2];
        JSValue promise = JS_NewPromiseCapability(ctx, resolving);
        if (JS_IsException(promise)) {
            return promise;
        }
        tb_session.pending[call_id].resolve = resolving[0];
        tb_session.pending[call_id].reject = resolving[1];
        tb_session.pending[call_id].in_use = 1;
        tb_session.pending_count++;
        return promise;
    }

    int32_t response_len = tb_host_response_len();
    if (response_len < 0) {
        return JS_ThrowInternalError(ctx, "tinysandbox host response failed");
    }

    char *response = js_malloc(ctx, (size_t)response_len + 1);
    if (!response) {
        return JS_EXCEPTION;
    }
    if (tb_host_response_read((uint8_t *)response, response_len) != response_len) {
        js_free(ctx, response);
        return JS_ThrowInternalError(ctx, "tinysandbox host response read failed");
    }
    response[response_len] = '\0';

    JSValue parsed = JS_ParseJSON(ctx, response, (size_t)response_len, "<tinysandbox-response>");
    js_free(ctx, response);
    return parsed;
}

static JSValue js_write_stdout(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    (void)this_val;
    if (argc == 0) {
        return JS_UNDEFINED;
    }
    size_t len = 0;
    const char *data = JS_ToCStringLen(ctx, &len, argv[0]);
    if (!data) {
        return JS_EXCEPTION;
    }
    tb_write_stdout((const uint8_t *)data, (int32_t)len);
    JS_FreeCString(ctx, data);
    return JS_UNDEFINED;
}

static JSValue js_write_stderr(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    (void)this_val;
    if (argc == 0) {
        return JS_UNDEFINED;
    }
    size_t len = 0;
    const char *data = JS_ToCStringLen(ctx, &len, argv[0]);
    if (!data) {
        return JS_EXCEPTION;
    }
    tb_write_stderr((const uint8_t *)data, (int32_t)len);
    JS_FreeCString(ctx, data);
    return JS_UNDEFINED;
}

static JSValue js_process_exit(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    (void)this_val;
    int32_t code = 0;
    if (argc > 0) {
        JS_ToInt32(ctx, &code, argv[0]);
    }
    JSValue exit = JS_NewError(ctx);
    JS_SetPropertyStr(ctx, exit, "__tinysandboxExit", JS_NewBool(ctx, 1));
    JS_SetPropertyStr(ctx, exit, "code", JS_NewInt32(ctx, code));
    JS_SetUncatchableError(ctx, exit);
    tb_session.exited = 1;
    return JS_Throw(ctx, exit);
}

static JSValue js_eval_module(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    (void)this_val;
    if (argc < 2) {
        return JS_ThrowTypeError(ctx, "module eval requires source and filename");
    }

    size_t source_len = 0;
    const char *source = JS_ToCStringLen(ctx, &source_len, argv[0]);
    const char *filename = JS_ToCString(ctx, argv[1]);
    if (!source || !filename) {
        if (source) {
            JS_FreeCString(ctx, source);
        }
        if (filename) {
            JS_FreeCString(ctx, filename);
        }
        return JS_EXCEPTION;
    }

    JSValue result = JS_Eval(ctx, source, source_len, filename, JS_EVAL_TYPE_GLOBAL);
    JS_FreeCString(ctx, source);
    JS_FreeCString(ctx, filename);
    return result;
}

static JSValue js_register_timers(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv)
{
    (void)this_val;
    if (argc < 3) {
        return JS_ThrowTypeError(ctx, "timer registration requires three hooks");
    }
    JS_FreeValue(ctx, tb_session.timer_delay);
    JS_FreeValue(ctx, tb_session.timer_begin);
    JS_FreeValue(ctx, tb_session.timer_run);
    tb_session.timer_delay = JS_DupValue(ctx, argv[0]);
    tb_session.timer_begin = JS_DupValue(ctx, argv[1]);
    tb_session.timer_run = JS_DupValue(ctx, argv[2]);
    return JS_UNDEFINED;
}

static void set_function(JSContext *ctx, const char *name, JSCFunction *func, int argc)
{
    JSValue global = JS_GetGlobalObject(ctx);
    JS_SetPropertyStr(ctx, global, name, JS_NewCFunction(ctx, func, name, argc));
    JS_FreeValue(ctx, global);
}


static void free_unhandled_rejection(JSContext *ctx, UnhandledRejection *entry)
{
    JS_FreeValue(ctx, entry->promise);
    JS_FreeValue(ctx, entry->reason);
    free(entry);
}

static void clear_unhandled_rejections(JSContext *ctx, UnhandledRejectionState *state)
{
    UnhandledRejection *entry = state->head;
    while (entry) {
        UnhandledRejection *next = entry->next;
        free_unhandled_rejection(ctx, entry);
        entry = next;
    }
    state->head = NULL;
    state->tail = NULL;
}

static void promise_rejection_tracker(JSContext *ctx, JSValueConst promise, JSValueConst reason, bool is_handled, void *opaque)
{
    UnhandledRejectionState *state = (UnhandledRejectionState *)opaque;
    if (is_handled) {
        UnhandledRejection *previous = NULL;
        UnhandledRejection *entry = state->head;
        while (entry) {
            if (JS_VALUE_GET_PTR(entry->promise) == JS_VALUE_GET_PTR(promise)) {
                if (previous) {
                    previous->next = entry->next;
                } else {
                    state->head = entry->next;
                }
                if (state->tail == entry) {
                    state->tail = previous;
                }
                free_unhandled_rejection(ctx, entry);
                break;
            }
            previous = entry;
            entry = entry->next;
        }
        return;
    }
    UnhandledRejection *entry = malloc(sizeof(*entry));
    if (!entry) {
        return;
    }
    entry->promise = JS_DupValue(ctx, promise);
    entry->reason = JS_DupValue(ctx, reason);
    entry->next = NULL;
    if (state->tail) {
        state->tail->next = entry;
    } else {
        state->head = entry;
    }
    state->tail = entry;
}

static int32_t exit_code_from_exception(JSContext *ctx, JSValueConst exception, int *is_exit)
{
    JSValue marker = JS_GetPropertyStr(ctx, exception, "__tinysandboxExit");
    *is_exit = JS_ToBool(ctx, marker);
    JS_FreeValue(ctx, marker);
    if (!*is_exit) {
        return 1;
    }

    int32_t code = 0;
    JSValue code_value = JS_GetPropertyStr(ctx, exception, "code");
    JS_ToInt32(ctx, &code, code_value);
    JS_FreeValue(ctx, code_value);
    return code;
}

static void write_exception(JSContext *ctx, JSValueConst exception)
{
    if (JS_IsError(exception)) {
        JSValue name_value = JS_GetPropertyStr(ctx, exception, "name");
        JSValue message_value = JS_GetPropertyStr(ctx, exception, "message");
        JSValue stack_value = JS_GetPropertyStr(ctx, exception, "stack");
        size_t name_len = 0;
        size_t message_len = 0;
        size_t stack_len = 0;
        const char *name = JS_ToCStringLen(ctx, &name_len, name_value);
        const char *message = JS_ToCStringLen(ctx, &message_len, message_value);
        const char *stack = JS_ToCStringLen(ctx, &stack_len, stack_value);

        if (name && name_len > 0) {
            tb_write_stderr((const uint8_t *)name, (int32_t)name_len);
            if (message && message_len > 0) {
                tb_write_stderr((const uint8_t *)": ", 2);
                tb_write_stderr((const uint8_t *)message, (int32_t)message_len);
            }
            tb_write_stderr((const uint8_t *)"\n", 1);
        }
        if (stack && stack_len > 0) {
            tb_write_stderr((const uint8_t *)stack, (int32_t)stack_len);
            tb_write_stderr((const uint8_t *)"\n", 1);
        }

        if (name) {
            JS_FreeCString(ctx, name);
        }
        if (message) {
            JS_FreeCString(ctx, message);
        }
        if (stack) {
            JS_FreeCString(ctx, stack);
        }
        JS_FreeValue(ctx, name_value);
        JS_FreeValue(ctx, message_value);
        JS_FreeValue(ctx, stack_value);
        return;
    }

    size_t len = 0;
    const char *text = JS_ToCStringLen(ctx, &len, exception);
    if (text) {
        tb_write_stderr((const uint8_t *)text, (int32_t)len);
        tb_write_stderr((const uint8_t *)"\n", 1);
        JS_FreeCString(ctx, text);
    }
}

static int32_t handle_exception(JSContext *ctx, JSValueConst exception)
{
    int is_exit = 0;
    int32_t code = exit_code_from_exception(ctx, exception, &is_exit);
    if (!is_exit) {
        write_exception(ctx, exception);
    }
    return code;
}

static int32_t handle_eval_result(JSContext *ctx, JSValue value)
{
    if (!JS_IsException(value)) {
        JS_FreeValue(ctx, value);
        return 0;
    }

    JSValue exception = JS_GetException(ctx);
    int32_t code = handle_exception(ctx, exception);
    JS_FreeValue(ctx, exception);
    JS_FreeValue(ctx, value);
    return code;
}

static int32_t drain_pending_jobs(JSRuntime *rt, JSContext *ctx, UnhandledRejectionState *rejections)
{
    JSContext *job_ctx = NULL;
    int err = 0;
    while ((err = JS_ExecutePendingJob(rt, &job_ctx)) > 0) {
    }
    if (err < 0) {
        JSContext *exception_ctx = job_ctx ? job_ctx : ctx;
        JSValue exception = JS_GetException(exception_ctx);
        int32_t code = handle_exception(exception_ctx, exception);
        JS_FreeValue(exception_ctx, exception);
        clear_unhandled_rejections(ctx, rejections);
        return code;
    }
    if (rejections->head) {
        write_exception(ctx, rejections->head->reason);
        clear_unhandled_rejections(ctx, rejections);
        return 1;
    }
    return 0;
}

/* Frees the guest. Every exit path funnels here so the runtime, context and
 * rejection list are released exactly once regardless of where it ended. */
static int32_t session_finish(int32_t code)
{
    JSContext *ctx = tb_session.ctx;
    for (int32_t i = 0; i < TB_MAX_PENDING; i++) {
        if (tb_session.pending[i].in_use) {
            JS_FreeValue(ctx, tb_session.pending[i].resolve);
            JS_FreeValue(ctx, tb_session.pending[i].reject);
            tb_session.pending[i].in_use = 0;
        }
    }
    tb_session.pending_count = 0;
    JS_FreeValue(ctx, tb_session.main);
    JS_FreeValue(ctx, tb_session.timer_delay);
    JS_FreeValue(ctx, tb_session.timer_begin);
    JS_FreeValue(ctx, tb_session.timer_run);
    tb_session.main = JS_UNDEFINED;
    tb_session.timer_delay = JS_UNDEFINED;
    tb_session.timer_begin = JS_UNDEFINED;
    tb_session.timer_run = JS_UNDEFINED;
    clear_unhandled_rejections(ctx, &tb_session.rejections);
    JS_FreeContext(ctx);
    JS_FreeRuntime(tb_session.rt);
    tb_session.ctx = NULL;
    tb_session.rt = NULL;
    tb_session.active = 0;
    return code;
}

/* Asks the glue how long the event loop may wait: milliseconds until the next
 * timer or immediate is due, or -1 when nothing but host calls can wake it. */
static int32_t timers_delay(int32_t *delay)
{
    JSContext *ctx = tb_session.ctx;
    JSValue host_pending = JS_NewBool(ctx, tb_session.pending_count > 0);
    JSValue result = JS_Call(ctx, tb_session.timer_delay, JS_UNDEFINED, 1, (JSValueConst *)&host_pending);
    if (JS_IsException(result)) {
        return handle_eval_result(ctx, result);
    }
    double ms = -1;
    JS_ToFloat64(ctx, &ms, result);
    JS_FreeValue(ctx, result);
    *delay = !(ms >= 0) ? -1 : ms >= INT32_MAX ? INT32_MAX : (int32_t)ms;
    return 0;
}

/* Runs one turn of the event loop: the timers due when it began, then the
 * immediates queued before its immediate phase, draining the job queue after
 * each callback as Node does. */
static int32_t run_timers(void)
{
    JSContext *ctx = tb_session.ctx;
    JSValue begun = JS_Call(ctx, tb_session.timer_begin, JS_UNDEFINED, 0, NULL);
    if (JS_IsException(begun)) {
        return handle_eval_result(ctx, begun);
    }
    JS_FreeValue(ctx, begun);
    for (;;) {
        JSValue ran = JS_Call(ctx, tb_session.timer_run, JS_UNDEFINED, 0, NULL);
        if (JS_IsException(ran)) {
            return handle_eval_result(ctx, ran);
        }
        int more = JS_ToBool(ctx, ran);
        JS_FreeValue(ctx, ran);
        if (!more) {
            return 0;
        }
        int32_t code = drain_pending_jobs(tb_session.rt, ctx, &tb_session.rejections);
        if (code != 0 || tb_session.exited) {
            return code;
        }
    }
}

/* Runs the event loop until the guest must wait or has nothing left to do.
 * Due timers run here; outstanding host calls or a timer not yet due park the
 * guest, and the host resumes it with tinysandbox_resolve or tinysandbox_wake.
 * While host calls are outstanding the guest parks even for a due timer, so a
 * stream of timers cannot keep their answers from being delivered. As in Node,
 * process.exit() ends the program at once, whatever its status: queued jobs
 * and timers never run and outstanding host calls are abandoned, and a program
 * that runs out of work while its top-level await is still pending exits with
 * 13. */
static int32_t session_settle(int32_t code)
{
    for (;;) {
        if (code == 0 && !tb_session.exited) {
            code = drain_pending_jobs(tb_session.rt, tb_session.ctx, &tb_session.rejections);
        }
        if (code != 0 || tb_session.exited) {
            return session_finish(code);
        }
        int32_t delay = -1;
        code = timers_delay(&delay);
        if (code != 0 || tb_session.exited) {
            return session_finish(code);
        }
        if (tb_session.pending_count > 0 || delay > 0) {
            tb_session.wake_delay = delay;
            return TB_SUSPENDED;
        }
        if (delay < 0) {
            break;
        }
        /* Timers that keep coming due never return to the host, so check the
         * deadline here as well as inside their callbacks. */
        if (tb_should_interrupt()) {
            return session_finish(1);
        }
        code = run_timers();
    }
    if (JS_PromiseState(tb_session.ctx, tb_session.main) == JS_PROMISE_PENDING) {
        static const char warning[] = "Warning: Detected unsettled top-level await\n";
        tb_write_stderr((const uint8_t *)warning, (int32_t)(sizeof(warning) - 1));
        return session_finish(13);
    }
    return session_finish(0);
}

/* How long the parked guest may sleep before tinysandbox_wake: milliseconds
 * until its next timer is due, or -1 when only a host call can resume it. */
__attribute__((export_name("tinysandbox_timer_delay")))
int32_t tinysandbox_timer_delay(void)
{
    return tb_session.active ? tb_session.wake_delay : -1;
}

/* Runs the timers that came due while the guest was parked and resumes it. */
__attribute__((export_name("tinysandbox_wake")))
int32_t tinysandbox_wake(void)
{
    if (!tb_session.active) {
        return 1;
    }
    return session_settle(run_timers());
}

/* Settles one deferred host call with the staged response and resumes. */
__attribute__((export_name("tinysandbox_resolve")))
int32_t tinysandbox_resolve(int32_t call_id)
{
    if (!tb_session.active || call_id < 0 || call_id >= TB_MAX_PENDING ||
        !tb_session.pending[call_id].in_use) {
        return 1;
    }
    JSContext *ctx = tb_session.ctx;

    JSValue response = JS_UNDEFINED;
    int32_t response_len = tb_host_response_len();
    if (response_len >= 0) {
        char *bytes = js_malloc(ctx, (size_t)response_len + 1);
        if (!bytes) {
            return session_finish(1);
        }
        if (tb_host_response_read((uint8_t *)bytes, response_len) != response_len) {
            js_free(ctx, bytes);
            return session_finish(1);
        }
        bytes[response_len] = '\0';
        response = JS_ParseJSON(ctx, bytes, (size_t)response_len, "<tinysandbox-response>");
        js_free(ctx, bytes);
    }

    /* The glue unwraps {value}/{error} identically for both paths, so an error
     * response resolves here and throws there rather than being rebuilt in C. */
    JSValue resolve = tb_session.pending[call_id].resolve;
    JSValue reject = tb_session.pending[call_id].reject;
    tb_session.pending[call_id].in_use = 0;
    tb_session.pending_count--;

    int32_t code = 0;
    if (JS_IsException(response)) {
        JSValue exception = JS_GetException(ctx);
        JSValue thrown = JS_Call(ctx, reject, JS_UNDEFINED, 1, (JSValueConst *)&exception);
        JS_FreeValue(ctx, thrown);
        JS_FreeValue(ctx, exception);
    } else {
        JSValue settled = JS_Call(ctx, resolve, JS_UNDEFINED, 1, (JSValueConst *)&response);
        if (JS_IsException(settled)) {
            code = handle_eval_result(ctx, JS_DupValue(ctx, settled));
        }
        JS_FreeValue(ctx, settled);
        JS_FreeValue(ctx, response);
    }
    JS_FreeValue(ctx, resolve);
    JS_FreeValue(ctx, reject);

    return session_settle(code);
}

__attribute__((export_name("tinysandbox_run")))
int32_t tinysandbox_run(const uint8_t *input, int32_t input_len, int32_t heap_limit)
{
    if (tb_session.active) {
        tb_write_stderr((const uint8_t *)"quickjs: a guest is already running\n", 36);
        return 1;
    }
    memset(&tb_session, 0, sizeof(tb_session));
    tb_session.main = JS_UNDEFINED;
    tb_session.timer_delay = JS_UNDEFINED;
    tb_session.timer_begin = JS_UNDEFINED;
    tb_session.timer_run = JS_UNDEFINED;

    JSRuntime *rt = JS_NewRuntime();
    if (!rt) {
        tb_write_stderr((const uint8_t *)"quickjs: failed to create runtime\n", 34);
        return 1;
    }
    if (heap_limit > 0) {
        JS_SetMemoryLimit(rt, (size_t)heap_limit);
    }
    JS_SetMaxStackSize(rt, TINYSANDBOX_QUICKJS_STACK_SIZE);
    JS_SetInterruptHandler(rt, quickjs_should_interrupt, NULL);
    JS_UpdateStackTop(rt);
    tb_session.rt = rt;
    /* Session-owned, not a stack local: the runtime keeps this address and now
     * outlives the call that installed it. */
    JS_SetHostPromiseRejectionTracker(rt, promise_rejection_tracker, &tb_session.rejections);

    JSContext *ctx = JS_NewContext(rt);
    if (!ctx) {
        JS_FreeRuntime(rt);
        tb_session.rt = NULL;
        tb_write_stderr((const uint8_t *)"quickjs: failed to create context\n", 34);
        return 1;
    }
    tb_session.ctx = ctx;
    tb_session.active = 1;

    set_function(ctx, "__tinysandbox_host_call", js_host_call, 3);
    set_function(ctx, "__tinysandbox_stdout", js_write_stdout, 1);
    set_function(ctx, "__tinysandbox_stderr", js_write_stderr, 1);
    set_function(ctx, "__tinysandbox_exit", js_process_exit, 1);
    set_function(ctx, "__tinysandbox_eval_module", js_eval_module, 2);
    set_function(ctx, "__tinysandbox_timers", js_register_timers, 3);

    JSValue config = JS_ParseJSON(ctx, (const char *)input, (size_t)input_len, "<tinysandbox-config>");
    if (JS_IsException(config)) {
        int32_t code = handle_eval_result(ctx, config);
        return session_finish(code);
    }

    JSValue global = JS_GetGlobalObject(ctx);
    JS_SetPropertyStr(ctx, global, "__tinysandboxConfig", JS_DupValue(ctx, config));
    JS_FreeValue(ctx, global);

    JSValue run_main = JS_Eval(ctx, TINYSANDBOX_GLUE, strlen(TINYSANDBOX_GLUE), "<tinysandbox>", JS_EVAL_TYPE_GLOBAL);
    if (JS_IsException(run_main)) {
        int32_t code = handle_eval_result(ctx, run_main);
        JS_FreeValue(ctx, config);
        return session_finish(code);
    }

    JSValue prelude_value = JS_GetPropertyStr(ctx, config, "prelude");
    size_t prelude_len = 0;
    const char *prelude = JS_ToCStringLen(ctx, &prelude_len, prelude_value);
    if (!prelude) {
        JS_FreeValue(ctx, prelude_value);
        JS_FreeValue(ctx, run_main);
        JS_FreeValue(ctx, config);
        tb_write_stderr((const uint8_t *)"quickjs: invalid prelude config\n", 32);
        return session_finish(1);
    }
    if (prelude_len > 0) {
        int32_t code = handle_eval_result(ctx, JS_Eval(ctx, prelude, prelude_len, "<prelude>", JS_EVAL_TYPE_GLOBAL));
        if (code == 0 && !tb_session.exited) {
            code = drain_pending_jobs(rt, ctx, &tb_session.rejections);
        }
        if (code != 0 || tb_session.exited) {
            JS_FreeCString(ctx, prelude);
            JS_FreeValue(ctx, prelude_value);
            JS_FreeValue(ctx, run_main);
            JS_FreeValue(ctx, config);
            return session_finish(code);
        }
    }
    JS_FreeCString(ctx, prelude);
    JS_FreeValue(ctx, prelude_value);

    JSValue source_value = JS_GetPropertyStr(ctx, config, "code");
    JSValue path_value = JS_GetPropertyStr(ctx, config, "scriptPath");
    size_t source_len = 0;
    const char *source = JS_ToCStringLen(ctx, &source_len, source_value);
    const char *script_path = JS_ToCString(ctx, path_value);
    if (!source || !script_path) {
        if (source) {
            JS_FreeCString(ctx, source);
        }
        if (script_path) {
            JS_FreeCString(ctx, script_path);
        }
        JS_FreeValue(ctx, source_value);
        JS_FreeValue(ctx, path_value);
        JS_FreeValue(ctx, run_main);
        JS_FreeValue(ctx, config);
        tb_write_stderr((const uint8_t *)"quickjs: invalid script config\n", 31);
        return session_finish(1);
    }

    JSValue run_args[2] = {
        JS_NewStringLen(ctx, source, source_len),
        JS_NewString(ctx, script_path),
    };
    JSValue main = JS_Call(ctx, run_main, JS_UNDEFINED, 2, run_args);
    int32_t code = 0;
    if (JS_IsException(main)) {
        code = handle_eval_result(ctx, main);
    } else {
        tb_session.main = main;
        /* Rejected before its first await: a synchronous throw, which ends the
         * program before any queued job runs. */
        if (JS_PromiseState(ctx, main) == JS_PROMISE_REJECTED) {
            JSValue reason = JS_PromiseResult(ctx, main);
            code = handle_exception(ctx, reason);
            JS_FreeValue(ctx, reason);
        }
    }
    /* Released before settling: suspending returns from this frame, so nothing
     * owned by it may still be live. */
    JS_FreeValue(ctx, run_args[0]);
    JS_FreeValue(ctx, run_args[1]);
    JS_FreeValue(ctx, run_main);
    JS_FreeCString(ctx, source);
    JS_FreeCString(ctx, script_path);
    JS_FreeValue(ctx, source_value);
    JS_FreeValue(ctx, path_value);
    JS_FreeValue(ctx, config);
    return session_settle(code);
}
