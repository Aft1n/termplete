'use strict';

const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const zlib = require('node:zlib');
const { targetFor } = require('../npm/lib/launcher');
const packageInfo = require('../npm/package.json');

const npmDir = path.resolve(__dirname, '../npm');
const fakeBinaries = {
  termplete: Buffer.from('#!/bin/sh\nprintf "termplete"\nfor arg do printf "|%s" "$arg"; done\nprintf "\\n"\n'),
  tp: Buffer.from('#!/bin/sh\nprintf "tp"\nfor arg do printf "|%s" "$arg"; done\nprintf "\\n"\n')
};

function octal(value, width) {
  return `${value.toString(8).padStart(width - 1, '0')}\0`;
}

function tarEntry(name, data, type = '0', linkName = '') {
  const header = Buffer.alloc(512);
  header.write(name, 0, 100, 'utf8');
  header.write(octal(0o755, 8), 100, 8, 'ascii');
  header.write(octal(0, 8), 108, 8, 'ascii');
  header.write(octal(0, 8), 116, 8, 'ascii');
  header.write(octal(data.length, 12), 124, 12, 'ascii');
  header.write(octal(0, 12), 136, 12, 'ascii');
  header.fill(0x20, 148, 156);
  header[156] = type.charCodeAt(0);
  header.write(linkName, 157, 100, 'utf8');
  header.write('ustar\0', 257, 6, 'ascii');
  header.write('00', 263, 2, 'ascii');
  const checksum = header.reduce((sum, byte) => sum + byte, 0);
  header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148, 8, 'ascii');
  const padding = Buffer.alloc((512 - (data.length % 512)) % 512);
  return Buffer.concat([header, data, padding]);
}

function tarGzip(entries) {
  return zlib.gzipSync(Buffer.concat([
    ...entries.map(([name, content]) => tarEntry(name, content)),
    Buffer.alloc(1024)
  ]));
}

function sha256(data) {
  return crypto.createHash('sha256').update(data).digest('hex');
}

function launch(cliPath, args, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [cliPath, ...args], { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8').on('data', chunk => { stdout += chunk; });
    child.stderr.setEncoding('utf8').on('data', chunk => { stderr += chunk; });
    child.on('error', reject);
    child.on('close', code => resolve({ code, stdout, stderr }));
  });
}

function interruptTwice(cliPath, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [cliPath], { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let output = '';
    let errors = '';
    let interrupts = 0;
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`Signal forwarding timed out: ${output} ${errors}`));
    }, 10000);
    child.stdout.setEncoding('utf8').on('data', chunk => {
      output += chunk;
      if (interrupts === 0 && output.includes('ready')) {
        interrupts += 1;
        child.kill('SIGINT');
      } else if (interrupts === 1 && output.includes('first interrupt')) {
        interrupts += 1;
        child.kill('SIGINT');
      }
    });
    child.stderr.setEncoding('utf8').on('data', chunk => { errors += chunk; });
    child.on('error', error => { clearTimeout(timer); reject(error); });
    child.on('close', code => {
      clearTimeout(timer);
      try {
        assert.equal(interrupts, 2, output);
        assert.equal(code, 42, `${output} ${errors}`);
        resolve();
      } catch (error) { reject(error); }
    });
  });
}

async function main() {
  const target = targetFor(
    process.platform,
    process.arch,
    process.platform !== 'linux' || Boolean(process.report?.getReport().header.glibcVersionRuntime)
  );
  assert.throws(() => targetFor('win32', 'x64'), /Windows is not supported/);
  assert.throws(() => targetFor('linux', 'x64', false), /musl/);

  const validArchive = tarGzip(Object.entries(fakeBinaries));
  const signalBinary = Buffer.from(`#!${process.execPath}\n` +
    'let count = 0; process.on("SIGINT", () => { if (++count === 2) process.exit(42); ' +
    'else process.stdout.write("first interrupt\\n"); }); ' +
    'process.stdout.write("ready\\n"); setInterval(() => {}, 1000);\n');
  const signalArchive = tarGzip([['termplete', signalBinary], ['tp', fakeBinaries.tp]]);
  const trailingArchive = zlib.gzipSync(Buffer.concat([
    zlib.gunzipSync(validArchive), tarEntry('../escape', Buffer.from('outside'))
  ]));
  const traversalArchive = tarGzip([
    ...Object.entries(fakeBinaries),
    ['../escape', Buffer.from('outside')]
  ]);
  const linkArchive = zlib.gzipSync(Buffer.concat([
    tarEntry('termplete', Buffer.alloc(0), '1', '/etc/passwd'),
    tarEntry('tp', fakeBinaries.tp),
    Buffer.alloc(1024)
  ]));
  const version = packageInfo.version;
  const asset = `termplete-${version}-${target}.tar.gz`;
  const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'termplete-npm-test-'));
  let mode = 'normal';
  const hits = { sums: 0, archive: 0 };
  const server = http.createServer((request, response) => {
    if (request.url.endsWith('/SHA256SUMS')) {
      hits.sums += 1;
      if (mode === 'offline') {
        response.writeHead(503).end('unavailable');
        return;
      }
      const checksumName = mode === 'version-mismatch'
        ? `termplete-0.0.9-${target}.tar.gz`
        : asset;
      const archiveForSum = mode === 'bad-checksum' ? Buffer.from('wrong archive') :
        mode === 'signal' ? signalArchive :
        mode === 'trailing-archive' ? trailingArchive :
        mode === 'path-traversal' ? traversalArchive :
        mode === 'archive-link' ? linkArchive : validArchive;
      const sum = mode === 'bad-checksum' ? '0'.repeat(64) : sha256(archiveForSum);
      response.writeHead(200, { 'content-type': 'text/plain' }).end(`${sum}  ${checksumName}\n`);
      return;
    }
    if (request.url.endsWith(`/${asset}`)) {
      hits.archive += 1;
      const archive = mode === 'path-traversal' ? traversalArchive :
        mode === 'signal' ? signalArchive :
        mode === 'trailing-archive' ? trailingArchive :
        mode === 'archive-link' ? linkArchive : validArchive;
      response.writeHead(200, { 'content-type': 'application/gzip' }).end(archive);
      return;
    }
    response.writeHead(404).end('not found');
  });

  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  const baseUrl = `http://127.0.0.1:${address.port}/v${version}`;
  const cliTermplete = path.join(npmDir, 'bin', 'termplete.js');
  const cliTp = path.join(npmDir, 'bin', 'tp.js');
  const baseEnv = {
    ...process.env,
    NODE_ENV: 'test',
    TERMPLETE_NPM_TEST_RELEASE_BASE_URL: baseUrl
  };
  delete baseEnv.TERMPLETE_NPM_CACHE_DIR;

  try {
    const cache = path.join(tempRoot, 'concurrent-cache');
    const env = { ...baseEnv, XDG_CACHE_HOME: cache };
    const [mainCommand, aliasCommand] = await Promise.all([
      launch(cliTermplete, ['setup', 'with spaces'], env),
      launch(cliTp, ['status'], env)
    ]);
    assert.equal(mainCommand.code, 0, mainCommand.stderr);
    assert.equal(aliasCommand.code, 0, aliasCommand.stderr);
    assert.match(mainCommand.stdout, /^termplete\|setup\|with spaces/);
    assert.match(aliasCommand.stdout, /^tp\|status/);
    assert.equal(hits.sums, 1, 'concurrent installs should download checksums once');
    assert.equal(hits.archive, 1, 'concurrent installs should download the archive once');

    const cachedRun = await launch(cliTp, ['again'], env);
    assert.equal(cachedRun.code, 0, cachedRun.stderr);
    assert.equal(hits.sums, 1, 'cached invocation should not access the network');

    const cachedBinary = path.join(cache, 'termplete', 'npm', version, target, 'termplete');
    fs.appendFileSync(cachedBinary, 'corrupt the cached executable');
    const repairedRun = await launch(cliTermplete, ['repaired'], env);
    assert.equal(repairedRun.code, 0, repairedRun.stderr);
    assert.match(repairedRun.stdout, /^termplete\|repaired/);
    assert.equal(hits.sums, 2, 'a corrupted cached executable should be downloaded again');

    mode = 'signal';
    await interruptTwice(cliTermplete, { ...baseEnv, XDG_CACHE_HOME: path.join(tempRoot, 'signals') });

    for (const [caseName, caseMode, expectedMessage] of [
      ['bad-checksum', 'bad-checksum', /SHA-256 verification failed/],
      ['version-mismatch', 'version-mismatch', new RegExp('exactly one entry for termplete-' + version.replace(/\./g, '\\.') + '-')],
      ['path-traversal', 'path-traversal', /only root-level termplete and tp/],
      ['archive-link', 'archive-link', /only regular executable files/],
      ['trailing-archive', 'trailing-archive', /only root-level termplete and tp/],
      ['offline', 'offline', /HTTP 503/]
    ]) {
      mode = caseMode;
      const caseEnv = { ...baseEnv, XDG_CACHE_HOME: path.join(tempRoot, caseName) };
      const result = await launch(cliTermplete, ['status'], caseEnv);
      assert.equal(result.code, 1, `${caseName} should fail: ${result.stdout} ${result.stderr}`);
      assert.match(result.stderr, expectedMessage);
      assert.equal(fs.existsSync(path.join(caseEnv.XDG_CACHE_HOME, 'termplete', 'npm', version, target, 'termplete')), false);
    }
  } finally {
    await new Promise(resolve => server.close(resolve));
    fs.rmSync(tempRoot, { recursive: true, force: true });
  }
}

main().then(() => {
  process.stdout.write('npm launcher harness passed\n');
}).catch(error => {
  process.stderr.write(`${error.stack || error}\n`);
  process.exitCode = 1;
});
