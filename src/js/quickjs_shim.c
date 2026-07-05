#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "quickjs.h"

#define TINYSANDBOX_QUICKJS_STACK_SIZE (3 * 1024 * 1024)

__attribute__((import_module("tinysandbox"), import_name("host_call")))
int32_t tb_host_call(const uint8_t *op, int32_t op_len, const uint8_t *json, int32_t json_len);

__attribute__((import_module("tinysandbox"), import_name("host_response_len")))
int32_t tb_host_response_len(void);

__attribute__((import_module("tinysandbox"), import_name("host_response_read")))
int32_t tb_host_response_read(uint8_t *ptr, int32_t len);

__attribute__((import_module("tinysandbox"), import_name("write_stdout")))
int32_t tb_write_stdout(const uint8_t *ptr, int32_t len);

__attribute__((import_module("tinysandbox"), import_name("write_stderr")))
int32_t tb_write_stderr(const uint8_t *ptr, int32_t len);

static const char TINYSANDBOX_GLUE[] =
"(() => {\n"
"  const hostCall = globalThis.__tinysandbox_host_call\n"
"  const writeStdout = globalThis.__tinysandbox_stdout\n"
"  const writeStderr = globalThis.__tinysandbox_stderr\n"
"  const exit = globalThis.__tinysandbox_exit\n"
"  const evalModule = globalThis.__tinysandbox_eval_module\n"
"  const config = globalThis.__tinysandboxConfig\n"
"  delete globalThis.__tinysandbox_host_call\n"
"  delete globalThis.__tinysandbox_stdout\n"
"  delete globalThis.__tinysandbox_stderr\n"
"  delete globalThis.__tinysandbox_exit\n"
"  delete globalThis.__tinysandbox_eval_module\n"
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
"  function call(op, args) {\n"
"    const response = hostCall(op, JSON.stringify(args === undefined ? null : args))\n"
"    if (response && response.error) throw fsError(response.error)\n"
"    return response ? response.value : undefined\n"
"  }\n"
"  function syscallError(e) {\n"
"    const err = new Error(e && e.message ? e.message : 'syscall failed')\n"
"    if (e && e.code !== undefined) err.code = e.code\n"
"    return err\n"
"  }\n"
"  function callSyscall(name, args) {\n"
"    const response = hostCall('syscall', JSON.stringify({ name, args: args === undefined ? null : args }))\n"
"    if (response && response.error) throw syscallError(response.error)\n"
"    return response ? response.value : undefined\n"
"  }\n"
"  if (Array.isArray(config.syscalls) && config.syscalls.length > 0) {\n"
"    // Null prototype keeps names like __proto__ as ordinary syscall properties.\n"
"    const sandboxObject = Object.create(null)\n"
"    for (const name of config.syscalls) {\n"
"      sandboxObject[name] = arg => callSyscall(name, arg)\n"
"    }\n"
"    globalThis.sandbox = sandboxObject\n"
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
"  function readFileSync(path, options) {\n"
"    const data = base64Decode(call('readFile', { path: pathValue(path) }))\n"
"    const enc = encodingFrom(options)\n"
"    const buffer = Buffer.from(data)\n"
"    return enc === 'utf8' ? buffer.toString(enc) : buffer\n"
"  }\n"
"  function writeFileSync(path, data, options) { call('writeFile', { path: pathValue(path), data: base64Encode(bytesFrom(data, options && options.encoding)) }) }\n"
"  function appendFileSync(path, data, options) { call('appendFile', { path: pathValue(path), data: base64Encode(bytesFrom(data, options && options.encoding)) }) }\n"
"  function mkdirSync(path, options) { return call('mkdir', { path: pathValue(path), recursive: recursive(options) }) }\n"
"  function readdirSync(path, options) {\n"
"    const entries = call('readdir', { path: pathValue(path), withFileTypes: withFileTypes(options) })\n"
"    return withFileTypes(options) ? entries.map(entry => new Dirent(entry)) : entries\n"
"  }\n"
"  function statSync(path) { return new Stats(call('stat', { path: pathValue(path) })) }\n"
"  function renameSync(oldPath, newPath) { call('rename', { from: pathValue(oldPath), to: pathValue(newPath) }) }\n"
"  function rmSync(path, options) { call('rm', { path: pathValue(path), recursive: recursive(options), force: force(options) }) }\n"
"  function unlinkSync(path) { call('unlink', { path: pathValue(path) }) }\n"
"  function rmdirSync(path, options) { call('rmdir', { path: pathValue(path), recursive: recursive(options) }) }\n"
"  function existsSync(path) { return !!call('exists', { path: pathValue(path) }) }\n"
"  function openSync(path, flags) { return call('open', { path: pathValue(path), flags: flags === undefined ? 'r' : String(flags) }) }\n"
"  function readSync(fd, buffer, offset, length, position) {\n"
"    offset = offset === undefined ? 0 : Number(offset)\n"
"    length = length === undefined ? buffer.length - offset : Number(length)\n"
"    const result = call('read', { fd, offset, length, position: position === null || position === undefined ? null : position })\n"
"    const data = base64Decode(result.data)\n"
"    for (let i = 0; i < data.length; i++) buffer[offset + i] = data[i]\n"
"    return result.bytesRead\n"
"  }\n"
"  function writeSync(fd, buffer, offset, length, position) {\n"
"    let data\n"
"    let writePosition\n"
"    if (typeof buffer === 'string') {\n"
"      data = bytesFrom(buffer, length)\n"
"      writePosition = offset === null || offset === undefined ? null : offset\n"
"    } else {\n"
"      offset = offset === undefined ? 0 : Number(offset)\n"
"      length = length === undefined ? buffer.length - offset : Number(length)\n"
"      data = typeof buffer.subarray === 'function' ? buffer.subarray(offset, offset + length) : Array.from(buffer).slice(offset, offset + length)\n"
"      writePosition = position === null || position === undefined ? null : position\n"
"    }\n"
"    return call('write', { fd, data: base64Encode(data), position: writePosition })\n"
"  }\n"
"  function ftruncateSync(fd, len) { call('ftruncate', { fd, len: len === undefined ? 0 : len }) }\n"
"  function closeSync(fd) { call('close', { fd }) }\n"
"  function copyFileSync(src, dest) { call('copyFile', { src: pathValue(src), dest: pathValue(dest) }) }\n"
"  function syncOnly(name) { return function() { throw new Error(`fs.${name} is not supported in tinysandbox: sync-only`) } }\n"
"  const fs = { readFileSync, writeFileSync, appendFileSync, mkdirSync, readdirSync, statSync, renameSync, rmSync, unlinkSync, rmdirSync, existsSync, openSync, readSync, writeSync, ftruncateSync, closeSync, copyFileSync,\n"
"    readFile: syncOnly('readFile'), writeFile: syncOnly('writeFile'), appendFile: syncOnly('appendFile'), mkdir: syncOnly('mkdir'), readdir: syncOnly('readdir'), stat: syncOnly('stat'), rename: syncOnly('rename'), rm: syncOnly('rm'), unlink: syncOnly('unlink'), rmdir: syncOnly('rmdir'), open: syncOnly('open'), read: syncOnly('read'), write: syncOnly('write'), ftruncate: syncOnly('ftruncate'), close: syncOnly('close'), copyFile: syncOnly('copyFile') }\n"
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
"      if (specifier === 'fs') return fs\n"
"      if (typeof specifier !== 'string') throw new TypeError(`The \"id\" argument must be of type string. Received ${typeof specifier}`)\n"
"      return loadModule(resolveModule(specifier, module), module)\n"
"    }\n"
"    Object.defineProperty(localRequire, 'main', { get: () => mainModule === null ? undefined : mainModule })\n"
"    localRequire.cache = moduleCache\n"
"    return localRequire\n"
"  }\n"
"  function executeJsModule(module, source, thisArg) {\n"
"    const wrapper = `(function (exports, require, module, __filename, __dirname) {${stripBomAndShebang(source)}\\n})`\n"
"    const compiled = evalModule(wrapper, module.filename)\n"
"    const thisValue = arguments.length < 3 ? module.exports : thisArg\n"
"    return compiled.call(thisValue, module.exports, module.require, module, module.filename, module.dirname)\n"
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
"      else executeJsModule(module, source)\n"
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
"    executeJsModule(module, source, isEval ? globalThis : module.exports)\n"
"    module.loaded = true\n"
"    globalThis.exports = module.exports\n"
"    return module.exports\n"
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
"  globalThis.Buffer = Buffer\n"
"  globalThis.Headers = Headers\n"
"  globalThis.Response = Response\n"
"  globalThis.fetch = fetch\n"
"  globalThis.console = { log: (...args) => writeStdout(line(args)), info: (...args) => writeStdout(line(args)), error: (...args) => writeStderr(line(args)), warn: (...args) => writeStderr(line(args)) }\n"
"  globalThis.process = { argv: config.argv, env: config.env, cwd: () => config.cwd, exit: code => exit(code === undefined ? 0 : Number(code) || 0) }\n"
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

    tb_host_call((const uint8_t *)op, (int32_t)op_len, (const uint8_t *)json, (int32_t)json_len);
    JS_FreeCString(ctx, op);
    JS_FreeCString(ctx, json);

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

static void set_function(JSContext *ctx, const char *name, JSCFunction *func, int argc)
{
    JSValue global = JS_GetGlobalObject(ctx);
    JS_SetPropertyStr(ctx, global, name, JS_NewCFunction(ctx, func, name, argc));
    JS_FreeValue(ctx, global);
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

__attribute__((export_name("tinysandbox_run")))
int32_t tinysandbox_run(const uint8_t *input, int32_t input_len)
{
    JSRuntime *rt = JS_NewRuntime();
    if (!rt) {
        tb_write_stderr((const uint8_t *)"quickjs: failed to create runtime\n", 34);
        return 1;
    }
    JS_SetMaxStackSize(rt, TINYSANDBOX_QUICKJS_STACK_SIZE);
    JS_UpdateStackTop(rt);
    UnhandledRejectionState rejections = { NULL, NULL };
    JS_SetHostPromiseRejectionTracker(rt, promise_rejection_tracker, &rejections);

    JSContext *ctx = JS_NewContext(rt);
    if (!ctx) {
        JS_FreeRuntime(rt);
        tb_write_stderr((const uint8_t *)"quickjs: failed to create context\n", 34);
        return 1;
    }

    set_function(ctx, "__tinysandbox_host_call", js_host_call, 2);
    set_function(ctx, "__tinysandbox_stdout", js_write_stdout, 1);
    set_function(ctx, "__tinysandbox_stderr", js_write_stderr, 1);
    set_function(ctx, "__tinysandbox_exit", js_process_exit, 1);
    set_function(ctx, "__tinysandbox_eval_module", js_eval_module, 2);

    JSValue config = JS_ParseJSON(ctx, (const char *)input, (size_t)input_len, "<tinysandbox-config>");
    if (JS_IsException(config)) {
        int32_t code = handle_eval_result(ctx, config);
        clear_unhandled_rejections(ctx, &rejections);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
        return code;
    }

    JSValue global = JS_GetGlobalObject(ctx);
    JS_SetPropertyStr(ctx, global, "__tinysandboxConfig", JS_DupValue(ctx, config));
    JS_FreeValue(ctx, global);

    JSValue run_main = JS_Eval(ctx, TINYSANDBOX_GLUE, strlen(TINYSANDBOX_GLUE), "<tinysandbox>", JS_EVAL_TYPE_GLOBAL);
    if (JS_IsException(run_main)) {
        int32_t code = handle_eval_result(ctx, run_main);
        clear_unhandled_rejections(ctx, &rejections);
        JS_FreeValue(ctx, config);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
        return code;
    }

    JSValue prelude_value = JS_GetPropertyStr(ctx, config, "prelude");
    size_t prelude_len = 0;
    const char *prelude = JS_ToCStringLen(ctx, &prelude_len, prelude_value);
    if (!prelude) {
        JS_FreeValue(ctx, prelude_value);
        JS_FreeValue(ctx, run_main);
        JS_FreeValue(ctx, config);
        clear_unhandled_rejections(ctx, &rejections);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
        tb_write_stderr((const uint8_t *)"quickjs: invalid prelude config\n", 32);
        return 1;
    }
    if (prelude_len > 0) {
        int32_t code = handle_eval_result(ctx, JS_Eval(ctx, prelude, prelude_len, "<prelude>", JS_EVAL_TYPE_GLOBAL));
        if (code == 0) {
            code = drain_pending_jobs(rt, ctx, &rejections);
        }
        if (code != 0) {
            JS_FreeCString(ctx, prelude);
            JS_FreeValue(ctx, prelude_value);
            JS_FreeValue(ctx, run_main);
            JS_FreeValue(ctx, config);
            clear_unhandled_rejections(ctx, &rejections);
            JS_FreeContext(ctx);
            JS_FreeRuntime(rt);
            return code;
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
        clear_unhandled_rejections(ctx, &rejections);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
        tb_write_stderr((const uint8_t *)"quickjs: invalid script config\n", 31);
        return 1;
    }

    JSValue run_args[2] = {
        JS_NewStringLen(ctx, source, source_len),
        JS_NewString(ctx, script_path),
    };
    int32_t code = handle_eval_result(ctx, JS_Call(ctx, run_main, JS_UNDEFINED, 2, run_args));
    if (code == 0) {
        code = drain_pending_jobs(rt, ctx, &rejections);
    }
    JS_FreeValue(ctx, run_args[0]);
    JS_FreeValue(ctx, run_args[1]);
    JS_FreeValue(ctx, run_main);
    JS_FreeCString(ctx, source);
    JS_FreeCString(ctx, script_path);
    JS_FreeValue(ctx, source_value);
    JS_FreeValue(ctx, path_value);
    JS_FreeValue(ctx, config);
    clear_unhandled_rejections(ctx, &rejections);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
    return code;
}
