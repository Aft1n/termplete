'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const os = require('node:os');
const path = require('node:path');
const zlib = require('node:zlib');
const { spawn, spawnSync } = require('node:child_process');
const packageInfo = require('../package.json');

const BINARIES = ['termplete', 'tp'];
const ARCHIVE_LIMIT = 128 * 1024 * 1024;
const CHECKSUMS_LIMIT = 1024 * 1024;
const LOCK_TIMEOUT_MS = 3 * 60 * 1000;
const STALE_LOCK_MS = 2 * 60 * 1000;

const TARGETS = {
  'darwin/arm64': 'aarch64-apple-darwin',
  'darwin/x64': 'x86_64-apple-darwin',
  'linux/arm64': 'aarch64-unknown-linux-gnu',
  'linux/x64': 'x86_64-unknown-linux-gnu'
};

function targetFor(platform, arch, glibc = true) {
  const target = TARGETS[`${platform}/${arch}`];
  if (!target || (platform === 'linux' && !glibc)) {
    const detail = platform === 'win32'
      ? 'Windows is not supported by this native release.'
      : `${platform}/${arch}${platform === 'linux' && !glibc ? ' (musl)' : ''} has no prebuilt release.`;
    throw new Error(
      `${detail} Supported targets are macOS and glibc-based Linux on x64 or ARM64. ` +
      'Install from source with Rust: cargo install --git https://github.com/aft1n/termplete --locked.'
    );
  }
  return target;
}

function supportedTarget() {
  const glibc = process.platform !== 'linux' || Boolean(
    process.report && process.report.getReport().header.glibcVersionRuntime
  );
  return targetFor(process.platform, process.arch, glibc);
}

function cacheRoot() {
  if (process.env.TERMPLETE_NPM_CACHE_DIR) {
    return path.resolve(process.env.TERMPLETE_NPM_CACHE_DIR);
  }

  const base = process.env.XDG_CACHE_HOME || (
    process.platform === 'darwin'
      ? path.join(os.homedir(), 'Library', 'Caches')
      : path.join(os.homedir(), '.cache')
  );
  return path.join(base, 'termplete', 'npm');
}

function releaseBase() {
  const testBase = process.env.TERMPLETE_NPM_TEST_RELEASE_BASE_URL;
  if (testBase && process.env.NODE_ENV === 'test') {
    const parsed = new URL(testBase);
    if (!['localhost', '127.0.0.1', '::1'].includes(parsed.hostname)) {
      throw new Error('The test release URL must point to localhost.');
    }
    return testBase.replace(/\/$/, '');
  }

  return `https://github.com/aft1n/termplete/releases/download/v${packageInfo.version}`;
}

function requestBuffer(url, maxBytes, redirects = 0) {
  if (redirects > 5) return Promise.reject(new Error('Too many redirects while downloading the native release.'));

  return new Promise((resolve, reject) => {
    let parsed;
    try {
      parsed = new URL(url);
    } catch (error) {
      reject(new Error(`Invalid release URL: ${error.message}`));
      return;
    }

    const transport = parsed.protocol === 'https:' ? https : parsed.protocol === 'http:' ? http : null;
    if (!transport) {
      reject(new Error('The release server used an unsupported URL protocol.'));
      return;
    }

    const request = transport.get(parsed, { headers: { 'user-agent': `termplete-npm/${packageInfo.version}` } }, response => {
      if ([301, 302, 303, 307, 308].includes(response.statusCode) && response.headers.location) {
        response.resume();
        const next = new URL(response.headers.location, parsed).toString();
        if (parsed.protocol === 'https:' && !next.startsWith('https://')) {
          reject(new Error('The release server attempted an insecure redirect.'));
          return;
        }
        requestBuffer(next, maxBytes, redirects + 1).then(resolve, reject);
        return;
      }

      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`Release download returned HTTP ${response.statusCode || 'error'}: ${parsed.pathname}`));
        return;
      }

      const chunks = [];
      let size = 0;
      response.on('data', chunk => {
        size += chunk.length;
        if (size > maxBytes) {
          request.destroy(new Error('The release download exceeded its size limit.'));
        } else {
          chunks.push(chunk);
        }
      });
      response.on('end', () => resolve(Buffer.concat(chunks)));
      response.on('error', reject);
    });

    request.setTimeout(30_000, () => request.destroy(new Error('Timed out while downloading the native release.')));
    request.on('error', reject);
  });
}

function checksumFor(manifest, filename) {
  const matches = manifest.toString('utf8').split(/\r?\n/).flatMap(line => {
    const match = /^([a-fA-F0-9]{64})\s+\*?(.+?)\s*$/.exec(line);
    return match && match[2] === filename ? [match[1].toLowerCase()] : [];
  });
  if (matches.length !== 1) {
    throw new Error(`The release SHA256SUMS file must contain exactly one entry for ${filename}.`);
  }
  return matches[0];
}

function parseOctal(field) {
  const value = field.toString('ascii').replace(/\0.*$/, '').trim();
  if (!/^[0-7]+$/.test(value)) throw new Error('The native archive contains an invalid tar header.');
  return Number.parseInt(value, 8);
}

function validateArchive(archive) {
  let contents;
  try {
    contents = zlib.gunzipSync(archive, { maxOutputLength: 256 * 1024 * 1024 });
  } catch (error) {
    throw new Error(`The native archive is not a valid gzip tar file: ${error.message}`);
  }

  const names = [];
  let offset = 0;
  while (offset + 512 <= contents.length) {
    const header = contents.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;

    const recordedChecksum = parseOctal(header.subarray(148, 156));
    let actualChecksum = 0;
    for (let index = 0; index < header.length; index += 1) {
      actualChecksum += index >= 148 && index < 156 ? 0x20 : header[index];
    }
    if (recordedChecksum !== actualChecksum) throw new Error('The native archive contains an invalid tar header.');

    const name = header.subarray(0, 100).toString('utf8').replace(/\0.*$/, '');
    const prefix = header.subarray(345, 500).toString('utf8').replace(/\0.*$/, '');
    const fullName = prefix ? `${prefix}/${name}` : name;
    const type = header[156];
    if (![0, '0'.charCodeAt(0)].includes(type)) {
      throw new Error('The native archive may contain only regular executable files.');
    }

    const size = parseOctal(header.subarray(124, 136));
    if (size > ARCHIVE_LIMIT) throw new Error('A native executable in the archive exceeded its size limit.');
    names.push(fullName);
    offset += 512 + Math.ceil(size / 512) * 512;
  }

  if (offset + 1024 > contents.length ||
      !contents.subarray(offset).every(byte => byte === 0) || names.length !== BINARIES.length ||
      new Set(names).size !== BINARIES.length || names.some(name => !BINARIES.includes(name))) {
    throw new Error('The native archive must contain only root-level termplete and tp executables.');
  }
}

function sha256File(filename) {
  return crypto.createHash('sha256').update(fs.readFileSync(filename)).digest('hex');
}

function isReady(installDir, target) {
  try {
    const manifest = JSON.parse(fs.readFileSync(path.join(installDir, 'install.json'), 'utf8'));
    if (manifest.version !== packageInfo.version || manifest.target !== target) return false;
    for (const binary of BINARIES) {
      const filename = path.join(installDir, binary);
      const stat = fs.lstatSync(filename);
      if (!stat.isFile() || stat.isSymbolicLink() || (stat.mode & 0o111) === 0) return false;
      if (sha256File(filename) !== manifest.sha256?.[binary]) return false;
    }
    return true;
  } catch {
    return false;
  }
}

function processIsAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === 'EPERM';
  }
}

function lockIsStale(lockDir) {
  try {
    const ownerPath = path.join(lockDir, 'owner.json');
    const stat = fs.statSync(lockDir);
    let owner;
    try {
      owner = JSON.parse(fs.readFileSync(ownerPath, 'utf8'));
    } catch {
      return Date.now() - stat.ctimeMs > 2 * 60 * 1000;
    }
    if (!Number.isInteger(owner.pid) || !Number.isFinite(owner.startedAt)) {
      return Date.now() - stat.ctimeMs > 2 * 60 * 1000;
    }
    return Date.now() - owner.startedAt > STALE_LOCK_MS && !processIsAlive(owner.pid);
  } catch {
    return false;
  }
}

function acquireLock(lockDir) {
  fs.mkdirSync(path.dirname(lockDir), { recursive: true });
  const deadline = Date.now() + LOCK_TIMEOUT_MS;
  while (Date.now() < deadline) {
    try {
      fs.mkdirSync(lockDir);
      fs.writeFileSync(path.join(lockDir, 'owner.json'), JSON.stringify({ pid: process.pid, startedAt: Date.now() }));
      return () => fs.rmSync(lockDir, { recursive: true, force: true });
    } catch (error) {
      if (error.code !== 'EEXIST') throw error;
      if (lockIsStale(lockDir)) {
        const staleName = `${lockDir}.stale-${process.pid}-${Date.now()}`;
        try {
          fs.renameSync(lockDir, staleName);
          fs.rmSync(staleName, { recursive: true, force: true });
        } catch {
          // Another installer may have replaced the stale lock; retry below.
        }
        continue;
      }
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
  throw new Error('Another TermPlete install is still running. Try the command again shortly.');
}

async function install(target) {
  const root = cacheRoot();
  const installDir = path.join(root, packageInfo.version, target);
  fs.mkdirSync(path.dirname(installDir), { recursive: true });

  const releaseLock = acquireLock(`${installDir}.lock`);
  try {
    if (isReady(installDir, target)) return installDir;

    const base = releaseBase();
    const asset = `termplete-${packageInfo.version}-${target}.tar.gz`;
    const sumsUrl = `${base}/SHA256SUMS`;
    const archiveUrl = `${base}/${asset}`;
    const sums = await requestBuffer(sumsUrl, CHECKSUMS_LIMIT);
    const expected = checksumFor(sums, asset);
    const archive = await requestBuffer(archiveUrl, ARCHIVE_LIMIT);
    const actual = crypto.createHash('sha256').update(archive).digest('hex');
    if (actual !== expected) {
      throw new Error(`SHA-256 verification failed for ${asset}; remove the partial cache and try again.`);
    }

    const stagingDir = fs.mkdtempSync(path.join(root, `.staging-${packageInfo.version}-${target}-`));
    const archivePath = path.join(stagingDir, asset);
    const unpackDir = path.join(stagingDir, 'unpacked');
    fs.mkdirSync(unpackDir);
    try {
      fs.writeFileSync(archivePath, archive, { mode: 0o600, flag: 'wx' });
      validateArchive(archive);
      const extracted = spawnSync('tar', ['-xzf', archivePath, '-C', unpackDir], { encoding: 'utf8', maxBuffer: 1024 * 1024 });
      if (extracted.error) throw new Error(`Could not extract the native archive: ${extracted.error.message}`);
      if (extracted.status !== 0) throw new Error(`The native archive could not be extracted: ${(extracted.stderr || '').trim()}`);

      const hashes = {};
      for (const binary of BINARIES) {
        const filename = path.join(unpackDir, binary);
        const stat = fs.lstatSync(filename);
        if (!stat.isFile() || stat.isSymbolicLink()) {
          throw new Error(`The native archive entry ${binary} is not a regular file.`);
        }
        fs.chmodSync(filename, 0o755);
        hashes[binary] = sha256File(filename);
      }

      fs.writeFileSync(path.join(unpackDir, 'install.json'), JSON.stringify({
        version: packageInfo.version,
        target,
        sha256: hashes
      }), { mode: 0o600, flag: 'wx' });
      fs.rmSync(installDir, { recursive: true, force: true });
      fs.renameSync(unpackDir, installDir);
      return installDir;
    } finally {
      fs.rmSync(stagingDir, { recursive: true, force: true });
    }
  } finally {
    releaseLock();
  }
}

function runBinary(filename, args) {
  const child = spawn(filename, args, { stdio: 'inherit' });
  const signals = ['SIGINT', 'SIGTERM', 'SIGHUP'];
  const handlers = new Map(signals.map(signal => [signal, () => {
    if (child.exitCode === null && child.signalCode === null) child.kill(signal);
  }]));
  for (const [signal, handler] of handlers) process.on(signal, handler);

  child.on('error', error => {
    for (const [signal, handler] of handlers) process.removeListener(signal, handler);
    console.error(`termplete: could not start the native executable: ${error.message}`);
    process.exitCode = 1;
  });
  child.on('close', (code, signal) => {
    for (const [name, handler] of handlers) process.removeListener(name, handler);
    if (signal) {
      process.exitCode = { SIGINT: 130, SIGTERM: 143, SIGHUP: 129 }[signal] || 1;
    } else {
      process.exitCode = code ?? 1;
    }
  });
}

async function run(binary) {
  if (Number(process.versions.node.split('.')[0]) < 20) {
    throw new Error('The TermPlete npm launcher requires Node.js 20 or newer.');
  }
  if (!BINARIES.includes(binary)) throw new Error(`Unknown native command: ${binary}`);

  const target = supportedTarget();
  const installDir = await install(target);
  runBinary(path.join(installDir, binary), process.argv.slice(2));
}

function launch(binary) {
  run(binary).catch(error => {
    console.error(`termplete: ${error.message}`);
    process.exitCode = 1;
  });
}

module.exports = { launch, run, targetFor, checksumFor, validateArchive };
