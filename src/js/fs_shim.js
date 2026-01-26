// Node.js fs module compatibility shim for PGlite
// This provides fs operations backed by Rust ops

const { ops } = Deno.core;

function createStats(stat) {
    return {
        isFile: () => stat.is_file,
        isDirectory: () => stat.is_directory,
        isSymbolicLink: () => stat.is_symlink,
        size: stat.size,
        mode: stat.mode,
        mtimeMs: stat.mtime_ms,
        atimeMs: stat.atime_ms,
        ctimeMs: stat.ctime_ms,
        mtime: new Date(stat.mtime_ms),
        atime: new Date(stat.atime_ms),
        ctime: new Date(stat.ctime_ms),
    };
}

export function existsSync(path) {
    return ops.op_fs_exists_sync(path);
}

export function mkdirSync(path, options) {
    const recursive = options?.recursive ?? false;
    ops.op_fs_mkdir_sync(path, recursive);
}

export function readFileSync(path, options) {
    const data = ops.op_fs_read_file_sync(path);
    if (options?.encoding === 'utf8' || options?.encoding === 'utf-8') {
        return new TextDecoder().decode(data);
    }
    return data;
}

export function writeFileSync(path, data, options) {
    let bytes;
    if (typeof data === 'string') {
        bytes = new TextEncoder().encode(data);
    } else if (data instanceof Uint8Array) {
        bytes = data;
    } else {
        bytes = new Uint8Array(data);
    }
    ops.op_fs_write_file_sync(path, bytes);
}

export function unlinkSync(path) {
    ops.op_fs_unlink_sync(path);
}

export function rmdirSync(path) {
    ops.op_fs_rmdir_sync(path);
}

export function statSync(path) {
    return createStats(ops.op_fs_stat_sync(path));
}

export function lstatSync(path) {
    return createStats(ops.op_fs_lstat_sync(path));
}

export function readdirSync(path) {
    return ops.op_fs_readdir_sync(path);
}

export function renameSync(oldPath, newPath) {
    ops.op_fs_rename_sync(oldPath, newPath);
}

export function truncateSync(path, len = 0) {
    ops.op_fs_truncate_sync(path, len);
}

export function chmodSync(path, mode) {
    // No-op - not critical for PGlite
}

export function utimesSync(path, atime, mtime) {
    // No-op - not critical for PGlite
}

// Default export for CommonJS-style require('fs')
export default {
    existsSync,
    mkdirSync,
    readFileSync,
    writeFileSync,
    unlinkSync,
    rmdirSync,
    statSync,
    lstatSync,
    readdirSync,
    renameSync,
    truncateSync,
    chmodSync,
    utimesSync,
};
