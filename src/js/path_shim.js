// Node.js path module compatibility shim for PGlite
const sep = '/';
const delimiter = ':';

export function join(...parts) {
    const joined = parts
        .filter(p => p && p.length > 0)
        .join(sep)
        .replace(/\/+/g, '/');
    return normalize(joined);
}

export function resolve(...parts) {
    const resolvedParts = [];
    let resolvedAbsolute = false;

    for (let i = parts.length - 1; i >= 0 && !resolvedAbsolute; i--) {
        const part = parts[i];
        if (part && part.length > 0) {
            resolvedParts.unshift(part);
            resolvedAbsolute = part.startsWith('/');
        }
    }

    const resolved = resolvedParts.join('/');
    return normalize(resolvedAbsolute ? resolved : '/' + resolved);
}

export function normalize(path) {
    if (!path) return '.';
    const isAbsolute = path.startsWith('/');
    const parts = path.split('/').filter(p => p && p !== '.');
    const result = [];
    for (const part of parts) {
        if (part === '..') {
            if (result.length > 0 && result[result.length - 1] !== '..') {
                result.pop();
            } else if (!isAbsolute) {
                result.push('..');
            }
        } else {
            result.push(part);
        }
    }
    let normalized = result.join('/');
    if (isAbsolute) normalized = '/' + normalized;
    return normalized || (isAbsolute ? '/' : '.');
}

export function dirname(path) {
    if (!path) return '.';
    const lastSlash = path.lastIndexOf('/');
    if (lastSlash === -1) return '.';
    if (lastSlash === 0) return '/';
    return path.slice(0, lastSlash);
}

export function basename(path, ext) {
    if (!path) return '';
    let base = path;
    const lastSlash = path.lastIndexOf('/');
    if (lastSlash !== -1) {
        base = path.slice(lastSlash + 1);
    }
    if (ext && base.endsWith(ext)) {
        base = base.slice(0, -ext.length);
    }
    return base;
}

export function extname(path) {
    if (!path) return '';
    const base = basename(path);
    const lastDot = base.lastIndexOf('.');
    if (lastDot <= 0) return '';
    return base.slice(lastDot);
}

export function isAbsolute(path) {
    return path && path.startsWith('/');
}

export function relative(from, to) {
    from = resolve(from);
    to = resolve(to);
    if (from === to) return '';

    const fromParts = from.split('/').filter(Boolean);
    const toParts = to.split('/').filter(Boolean);

    let commonLength = 0;
    for (let i = 0; i < Math.min(fromParts.length, toParts.length); i++) {
        if (fromParts[i] !== toParts[i]) break;
        commonLength++;
    }

    const upCount = fromParts.length - commonLength;
    const remainingTo = toParts.slice(commonLength);

    return [...Array(upCount).fill('..'), ...remainingTo].join('/') || '.';
}

export function parse(path) {
    const root = isAbsolute(path) ? '/' : '';
    const dir = dirname(path);
    const base = basename(path);
    const ext = extname(path);
    const name = base.slice(0, base.length - ext.length);
    return { root, dir, base, ext, name };
}

export function format(pathObject) {
    const dir = pathObject.dir || pathObject.root || '';
    const base = pathObject.base || (pathObject.name || '') + (pathObject.ext || '');
    return dir ? (dir === '/' ? dir + base : dir + '/' + base) : base;
}

export { sep, delimiter };

export default {
    join,
    resolve,
    normalize,
    dirname,
    basename,
    extname,
    isAbsolute,
    relative,
    parse,
    format,
    sep,
    delimiter,
};
