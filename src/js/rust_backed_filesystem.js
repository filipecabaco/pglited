// RustBackedFilesystem - Custom filesystem extending PGlite's BaseFilesystem
// This class bridges PGlite's VFS to the host filesystem via Rust ops

const ENOENT = 44;
const EEXIST = 20;
const textDecoder = new TextDecoder();
const textEncoder = new TextEncoder();

function joinPath(base, child) {
    if (!base) return child;
    if (!child) return base;
    const baseEnds = base.endsWith('/');
    const childStarts = child.startsWith('/');
    if (baseEnds && childStarts) return base.slice(0, -1) + child;
    if (!baseEnds && !childStarts) return base + '/' + child;
    return base + child;
}

function makeError(kind, path, code) {
    const err = new Error(kind + ': ' + path);
    err.code = code;
    return err;
}

export function createRustBackedFilesystem(BaseFilesystem, ops, dataDir) {
    return class RustBackedFilesystem extends BaseFilesystem {
        constructor() {
            super(dataDir, { debug: false });
            this._dataDir = dataDir;
            this._ops = ops;
        }

        async init(pg, emscriptenOptions) {
            console.log('[RustBackedFilesystem] init called with pg:', pg);
            console.log('[RustBackedFilesystem] emscriptenOptions:', emscriptenOptions);
            try {
                const result = await super.init(pg, emscriptenOptions);
                console.log('[RustBackedFilesystem] super.init completed');
                return result;
            } catch (err) {
                console.error('[RustBackedFilesystem] init error:', err);
                if (err && err.stack) console.error('[RustBackedFilesystem] error stack:', err.stack);
                throw err;
            }
        }

        async initialSyncFs() {
            // Nothing to sync - reads directly from disk
        }

        async syncToFs(relaxedDurability) {
            // All writes are synchronous to disk
        }

        async closeFs() {
            // No cleanup needed
        }

        // Map VFS path to actual filesystem path
        _mapPath(vfsPath) {
            return joinPath(this._dataDir, vfsPath);
        }

        chmod(path, mode) {
            // No-op
        }

        close(fd) {
            this._ops.op_fs_close_sync(fd);
        }

        fstat(fd) {
            return this._toFsStats(this._ops.op_fs_fstat_sync(fd));
        }

        lstat(path) {
            const realPath = this._mapPath(path);
            if (!this._ops.op_fs_exists_sync(realPath)) {
                throw makeError('ENOENT', realPath, ENOENT);
            }
            return this._toFsStats(this._ops.op_fs_lstat_sync(realPath));
        }

        mkdir(path, options) {
            const realPath = this._mapPath(path);
            const recursive = options?.recursive ?? false;
            if (!recursive) {
                const parts = realPath.split('/');
                const parentPath = parts.slice(0, -1).join('/');
                if (parentPath && !this._ops.op_fs_exists_sync(parentPath)) {
                    throw makeError('ENOENT', parentPath, ENOENT);
                }
            }
            if (this._ops.op_fs_exists_sync(realPath)) {
                throw makeError('EEXIST', realPath, EEXIST);
            }
            this._ops.op_fs_mkdir_sync(realPath, recursive);
        }

        open(path, flags = 'r', mode = 0o644) {
            const realPath = this._mapPath(path);
            let flagStr = String(flags);
            // Use r+ for existing files since VFS may need write access
            if (flagStr === 'r' && this._ops.op_fs_exists_sync(realPath)) {
                flagStr = 'r+';
            }
            if (flagStr === 'r' && !this._ops.op_fs_exists_sync(realPath)) {
                throw makeError('ENOENT', realPath, ENOENT);
            }
            return this._ops.op_fs_open_sync(realPath, flagStr, mode);
        }

        readdir(path) {
            const realPath = this._mapPath(path);
            if (!this._ops.op_fs_exists_sync(realPath)) {
                throw makeError('ENOENT', realPath, ENOENT);
            }
            return this._ops.op_fs_readdir_sync(realPath);
        }

        read(fd, buffer, offset, length, position) {
            if (length === 0) return 0;
            if (buffer instanceof Uint8Array) {
                const view = buffer.subarray(offset, offset + length);
                return this._ops.op_fs_read_fd_sync(fd, view, 0, length, BigInt(position));
            }
            const tempBuffer = new Uint8Array(length);
            const bytesRead = this._ops.op_fs_read_fd_sync(fd, tempBuffer, 0, length, BigInt(position));
            if (bytesRead > 0 && buffer?.set) {
                buffer.set(tempBuffer.subarray(0, bytesRead), offset);
            }
            return bytesRead;
        }

        rename(oldPath, newPath) {
            this._ops.op_fs_rename_sync(this._mapPath(oldPath), this._mapPath(newPath));
        }

        rmdir(path) {
            this._ops.op_fs_rmdir_sync(this._mapPath(path));
        }

        truncate(path, len) {
            this._ops.op_fs_truncate_sync(this._mapPath(path), BigInt(len));
        }

        unlink(path) {
            this._ops.op_fs_unlink_sync(this._mapPath(path));
        }

        utimes(path, atime, mtime) {
            // No-op
        }

        writeFile(path, data, options) {
            const realPath = this._mapPath(path);
            const parts = realPath.split('/');
            const parentPath = parts.slice(0, -1).join('/');
            if (parentPath && !this._ops.op_fs_exists_sync(parentPath)) {
                this._ops.op_fs_mkdir_sync(parentPath, true);
            }
            let bytes;
            if (typeof data === 'string') {
                bytes = textEncoder.encode(data);
            } else if (data instanceof Uint8Array) {
                bytes = data;
            } else if (data?.buffer) {
                bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
            } else {
                bytes = new Uint8Array(data || []);
            }
            this._ops.op_fs_write_file_sync(realPath, bytes);
        }

        write(fd, buffer, offset, length, position) {
            if (length === 0) return 0;
            let dataToWrite;
            if (buffer.buffer) {
                dataToWrite = new Uint8Array(buffer.buffer, buffer.byteOffset + offset, length);
            } else {
                dataToWrite = new Uint8Array(buffer.slice(offset, offset + length));
            }
            return this._ops.op_fs_write_fd_sync(fd, dataToWrite, 0, length, BigInt(position));
        }

        _toFsStats(stat) {
            return {
                dev: 0,
                ino: 0,
                mode: stat.mode,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: 0,
                size: stat.size,
                blksize: 4096,
                blocks: Math.ceil(stat.size / 512),
                atime: stat.atime_ms / 1000,
                mtime: stat.mtime_ms / 1000,
                ctime: stat.ctime_ms / 1000,
            };
        }
    };
}

/**
 * Extracts a tar file to the given directory using ops.
 * @param {Uint8Array} tarData - The tar file data
 * @param {string} dataDir - The target directory
 * @param {object} ops - Deno.core.ops object
 */
export function extractTar(tarData, dataDir, ops) {
    console.log('[extractTar] Starting extraction, tarData size:', tarData.length, 'targetDir:', dataDir);
    let offset = 0;
    let fileCount = 0;
    let dirCount = 0;

    try {
        while (offset < tarData.length) {
            if (offset + 512 > tarData.length) {
                console.log('[extractTar] Reached end of tar at offset:', offset);
                break;
            }
            if (tarData[offset] === 0 && tarData[offset + 100] === 0) {
                console.log('[extractTar] End of tar detected at offset:', offset);
                break;
            }

            const header = tarData.slice(offset, offset + 512);

            let nameEnd = 0;
            while (nameEnd < 100 && header[nameEnd] !== 0) nameEnd++;
            const name = textDecoder.decode(header.slice(0, nameEnd));
            if (!name) break;

            let sizeStr = '';
            for (let i = 124; i < 136 && header[i] !== 0 && header[i] !== 32; i++) {
                sizeStr += String.fromCharCode(header[i]);
            }
            const size = parseInt(sizeStr, 8) || 0;

            const type = header[156] - 48;
            const dataStart = offset + 512;
            const data = size > 0 ? tarData.slice(dataStart, dataStart + size) : new Uint8Array(0);
            const fullPath = joinPath(dataDir, name);

            if (fileCount < 5 || dirCount < 5) {
                console.log('[extractTar] File:', name, 'type:', type, 'size:', size, 'fullPath:', fullPath);
            }

            if (type === 5) {
                if (!ops.op_fs_exists_sync(fullPath)) {
                    ops.op_fs_mkdir_sync(fullPath, true);
                    dirCount++;
                }
            } else if (type === 0) {
                const parts = fullPath.split('/');
                const parentPath = parts.slice(0, -1).join('/');
                if (parentPath && !ops.op_fs_exists_sync(parentPath)) {
                    ops.op_fs_mkdir_sync(parentPath, true);
                }
                ops.op_fs_write_file_sync(fullPath, data);
                fileCount++;
            }

            offset = dataStart + Math.ceil(size / 512) * 512;
        }
        console.log('[extractTar] Complete. Extracted', dirCount, 'dirs and', fileCount, 'files');
    } catch (err) {
        console.error('[extractTar] Error during extraction:', err);
        if (err && err.stack) console.error('[extractTar] Stack:', err.stack);
        throw err;
    }
}
