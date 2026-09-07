import { VfsError } from "../dist/index.js";

const encoder = new TextEncoder();

function parent(path) {
  const index = path.lastIndexOf("/");
  return index <= 0 ? "/" : path.slice(0, index);
}

function name(path) {
  return path.slice(path.lastIndexOf("/") + 1);
}

export class TestVfs {
  constructor(files = {}) {
    this.calls = [];
    this.nodes = new Map([["/", { fileType: "directory" }]]);
    this.handles = new Map();
    this.nextHandle = 1;
    for (const [path, value] of Object.entries(files)) this.seed(path, value);
  }

  seed(path, value) {
    const parts = path.split("/").filter(Boolean);
    let current = "";
    for (const part of parts.slice(0, -1)) {
      current += `/${part}`;
      if (!this.nodes.has(current)) this.nodes.set(current, { fileType: "directory" });
    }
    const data = typeof value === "string" ? encoder.encode(value) : Uint8Array.from(value);
    this.nodes.set(path, { fileType: "file", data });
  }

  record(method, ...args) {
    this.calls.push({ method, args });
  }

  node(path) {
    const node = this.nodes.get(path);
    if (!node) throw new VfsError("ENOENT");
    return node;
  }

  stat(path) {
    this.record("stat", path);
    const node = this.node(path);
    return { fileType: node.fileType, len: node.fileType === "file" ? node.data.byteLength : 0 };
  }

  readdir(path) {
    this.record("readdir", path);
    if (this.node(path).fileType !== "directory") throw new VfsError("ENOTDIR");
    const prefix = path === "/" ? "/" : `${path}/`;
    return [...this.nodes.entries()]
      .filter(([candidate]) => candidate.startsWith(prefix) && candidate !== path && !candidate.slice(prefix.length).includes("/"))
      .map(([candidate, node]) => ({ name: name(candidate), metadata: { fileType: node.fileType, len: node.fileType === "file" ? node.data.byteLength : 0 } }))
      .sort((left, right) => left.name.localeCompare(right.name));
  }

  mkdir(path) {
    this.record("mkdir", path);
    if (this.nodes.has(path)) throw new VfsError("EEXIST");
    if (this.node(parent(path)).fileType !== "directory") throw new VfsError("ENOTDIR");
    this.nodes.set(path, { fileType: "directory" });
  }

  rename(from, to) {
    this.record("rename", from, to);
    this.node(from);
    if (this.nodes.has(to)) throw new VfsError("EEXIST");
    if (this.node(parent(to)).fileType !== "directory") throw new VfsError("ENOTDIR");
    const moved = [...this.nodes.entries()].filter(([path]) => path === from || path.startsWith(`${from}/`));
    for (const [path] of moved) this.nodes.delete(path);
    for (const [path, node] of moved) this.nodes.set(`${to}${path.slice(from.length)}`, node);
  }

  unlink(path) {
    this.record("unlink", path);
    if (this.node(path).fileType === "directory") throw new VfsError("EISDIR");
    this.nodes.delete(path);
  }

  rmdir(path) {
    this.record("rmdir", path);
    if (this.node(path).fileType !== "directory") throw new VfsError("ENOTDIR");
    if ([...this.nodes.keys()].some(candidate => candidate.startsWith(`${path}/`))) throw new VfsError("ENOTEMPTY");
    if (path === "/") throw new VfsError("EBUSY");
    this.nodes.delete(path);
  }

  open(path, mode) {
    this.record("open", path, mode);
    let node = this.nodes.get(path);
    if (node && node.fileType === "directory") throw new VfsError("EISDIR");
    if (node && mode.createNew) throw new VfsError("EEXIST");
    if (!node) {
      if (!mode.create) throw new VfsError("ENOENT");
      if (this.node(parent(path)).fileType !== "directory") throw new VfsError("ENOTDIR");
      node = { fileType: "file", data: new Uint8Array() };
      this.nodes.set(path, node);
    }
    if (mode.truncate) node.data = new Uint8Array();
    const handle = this.nextHandle++;
    this.handles.set(handle, { path, mode, node });
    return handle;
  }

  opened(handle, access) {
    const opened = this.handles.get(handle);
    if (!opened) throw new VfsError("EBADF");
    if (!opened.mode[access]) throw new VfsError("EBADF");
    return opened;
  }

  readAt(handle, offset, buffer) {
    this.record("readAt", handle, offset, buffer.byteLength);
    const opened = this.opened(handle, "read");
    const data = opened.node.data;
    const count = Math.min(buffer.byteLength, Math.max(0, data.byteLength - offset));
    buffer.set(data.subarray(offset, offset + count));
    return count;
  }

  writeAt(handle, offset, data) {
    this.record("writeAt", handle, offset, data.byteLength);
    const opened = this.opened(handle, "write");
    const node = opened.node;
    const writeOffset = opened.mode.append ? node.data.byteLength : offset;
    const output = new Uint8Array(Math.max(node.data.byteLength, writeOffset + data.byteLength));
    output.set(node.data);
    output.set(data, writeOffset);
    node.data = output;
    return data.byteLength;
  }

  truncate(handle, len) {
    this.record("truncate", handle, len);
    const opened = this.opened(handle, "write");
    const node = opened.node;
    const output = new Uint8Array(len);
    output.set(node.data.subarray(0, len));
    node.data = output;
  }

  close(handle) {
    this.record("close", handle);
    if (!this.handles.delete(handle)) throw new VfsError("EBADF");
  }
}
