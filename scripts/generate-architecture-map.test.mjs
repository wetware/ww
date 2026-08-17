import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import {
  DEFAULT_METADATA_PATH,
  DEFAULT_TEMPLATE_PATH,
  REPOSITORY_ROOT,
  generate,
  parseArguments,
  renderPage,
  serializeForScript,
  validateMetadata,
  validateSourcePath
} from './generate-architecture-map.mjs';

const REVISION = '0123456789abcdef0123456789abcdef01234567';
const metadata = JSON.parse(fs.readFileSync(DEFAULT_METADATA_PATH, 'utf8'));

function copyMetadata() {
  return structuredClone(metadata);
}

function temporaryDirectory(prefix = 'ww-architecture-map-') {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  return directory;
}

function removeDirectory(directory) {
  fs.rmSync(directory, { recursive: true, force: true });
}

function expectFailure(callback, pattern) {
  assert.throws(callback, error => error instanceof Error && pattern.test(error.message));
}

test('generates one standalone HTML artifact with the map and revision', t => {
  const outputDirectory = temporaryDirectory();
  t.after(() => removeDirectory(outputDirectory));

  const outputPath = generate({ outputDir: outputDirectory, revision: REVISION });
  const entries = fs.readdirSync(outputDirectory);
  const page = fs.readFileSync(outputPath, 'utf8');

  assert.deepEqual(entries, ['index.html']);
  assert.match(page, /MASTER @ 01234567/);
  assert.match(page, /"nodes":\[/);
  assert.equal((page.match(/"key":/g) ?? []).length, 13);
  assert.equal((page.match(/"style":/g) ?? []).length, 15);
  assert.doesNotMatch(page, /__ARCHITECTURE_MAP_(?:DATA|REVISION)__/);
  assert.doesNotMatch(page, /innerHTML/);
});

test('the generated JavaScript parses', t => {
  const outputDirectory = temporaryDirectory();
  t.after(() => removeDirectory(outputDirectory));
  const outputPath = generate({ outputDir: outputDirectory, revision: REVISION });
  const page = fs.readFileSync(outputPath, 'utf8');
  const script = page.match(/<script>\n([\s\S]*)\n  <\/script>/)?.[1];
  assert.ok(script, 'generated page contains a script');
  const scriptPath = path.join(outputDirectory, 'architecture-map.js');
  fs.writeFileSync(scriptPath, script);
  const result = spawnSync(process.execPath, ['--check', scriptPath], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
});

test('requires output and a full lowercase revision', () => {
  expectFailure(() => parseArguments([]), /usage/);
  expectFailure(() => parseArguments(['--output', 'site', '--revision', 'abc']), /40-character lowercase Git SHA/);
  expectFailure(() => parseArguments(['--output', 'site', '--output', 'other', '--revision', REVISION]), /usage/);
  expectFailure(() => parseArguments(['--unknown', 'site', '--revision', REVISION]), /usage/);
  assert.deepEqual(parseArguments(['--revision', REVISION, '--output', 'site']), { outputDir: 'site', revision: REVISION });
});

test('rejects malformed root and schema versions', () => {
  expectFailure(() => validateMetadata(null), /metadata root/);
  const invalid = copyMetadata();
  invalid.schemaVersion = 2;
  expectFailure(() => validateMetadata(invalid), /schemaVersion/);

  const missingText = copyMetadata();
  missingText.nodes[0].title = '';
  expectFailure(() => validateMetadata(missingText), /title must be a non-empty string/);

  const emptyFiles = copyMetadata();
  emptyFiles.nodes[0].files = [];
  expectFailure(() => validateMetadata(emptyFiles), /files must be a non-empty array/);
});

test('rejects duplicate keys and invalid node geometry', () => {
  const duplicate = copyMetadata();
  duplicate.nodes[1].key = duplicate.nodes[0].key;
  expectFailure(() => validateMetadata(duplicate), /duplicated/);

  const invalidGeometry = copyMetadata();
  invalidGeometry.nodes[0].w = 0;
  expectFailure(() => validateMetadata(invalidGeometry), /positive finite number/);
  invalidGeometry.nodes[0].w = Number.NaN;
  expectFailure(() => validateMetadata(invalidGeometry), /positive finite number/);
});

test('rejects route endpoints and route styles outside the contract', () => {
  const unknownEndpoint = copyMetadata();
  unknownEndpoint.routes[0].to = 'unknown';
  expectFailure(() => validateMetadata(unknownEndpoint), /unknown node/);

  const invalidStyle = copyMetadata();
  invalidStyle.routes[0].style = 'dotted';
  expectFailure(() => validateMetadata(invalidStyle), /primary or secondary/);
});

test('rejects missing, absolute, traversal, and directory source paths', () => {
  const missing = copyMetadata();
  missing.nodes[0].files = ['missing.rs'];
  expectFailure(() => validateMetadata(missing), /does not exist/);

  const absolute = copyMetadata();
  absolute.nodes[0].files = ['/tmp/source.rs'];
  expectFailure(() => validateMetadata(absolute), /must be relative/);

  const traversal = copyMetadata();
  traversal.nodes[0].files = ['../outside.rs'];
  expectFailure(() => validateMetadata(traversal), /escapes the repository/);

  const directory = copyMetadata();
  directory.nodes[0].files = ['src'];
  expectFailure(() => validateMetadata(directory), /regular file/);
});

test('rejects source symbolic links', t => {
  const rootDirectory = temporaryDirectory();
  t.after(() => removeDirectory(rootDirectory));
  fs.writeFileSync(path.join(rootDirectory, 'source.rs'), 'fn main() {}');
  fs.symlinkSync(path.join(rootDirectory, 'source.rs'), path.join(rootDirectory, 'source-link.rs'));
  expectFailure(() => validateSourcePath('source-link.rs', rootDirectory), /symbolic link/);
});

test('escapes metadata before insertion into a script', () => {
  const serialized = serializeForScript({ value: '</script>&\u2028\u2029' });
  assert.match(serialized, /\\u003c\/script\\u003e\\u0026\\u2028\\u2029/);
  assert.doesNotMatch(serialized, /<\/script>/);
});

test('requires exactly one data marker and revision marker', () => {
  expectFailure(() => renderPage('<script>__ARCHITECTURE_MAP_DATA__</script>', metadata, REVISION), /revision marker/);
  expectFailure(() => renderPage('__ARCHITECTURE_MAP_DATA__ __ARCHITECTURE_MAP_DATA__ __ARCHITECTURE_MAP_REVISION__', metadata, REVISION), /data marker/);
  expectFailure(() => renderPage('__ARCHITECTURE_MAP_DATA__ __ARCHITECTURE_MAP_REVISION__ __ARCHITECTURE_MAP_REVISION__', metadata, REVISION), /revision marker/);
});

test('does not create index.html when metadata validation fails', t => {
  const outputDirectory = temporaryDirectory();
  const fixtureDirectory = temporaryDirectory();
  t.after(() => { removeDirectory(outputDirectory); removeDirectory(fixtureDirectory); });
  const invalid = copyMetadata();
  invalid.nodes[0].files = ['missing.rs'];
  const metadataPath = path.join(fixtureDirectory, 'metadata.json');
  fs.writeFileSync(metadataPath, JSON.stringify(invalid));

  expectFailure(() => generate({ outputDir: outputDirectory, revision: REVISION, metadataPath, templatePath: DEFAULT_TEMPLATE_PATH }), /does not exist/);
  assert.equal(fs.existsSync(path.join(outputDirectory, 'index.html')), false);
});

test('does not create index.html when metadata or template parsing fails', t => {
  const outputDirectory = temporaryDirectory();
  const fixtureDirectory = temporaryDirectory();
  t.after(() => { removeDirectory(outputDirectory); removeDirectory(fixtureDirectory); });
  const malformedMetadataPath = path.join(fixtureDirectory, 'metadata.json');
  const malformedTemplatePath = path.join(fixtureDirectory, 'template.html');
  fs.writeFileSync(malformedMetadataPath, '{');
  fs.writeFileSync(malformedTemplatePath, '<script>__ARCHITECTURE_MAP_DATA__</script>');

  expectFailure(() => generate({ outputDir: outputDirectory, revision: REVISION, metadataPath: malformedMetadataPath }), /cannot parse metadata/);
  assert.equal(fs.existsSync(path.join(outputDirectory, 'index.html')), false);

  expectFailure(() => generate({ outputDir: outputDirectory, revision: REVISION, templatePath: malformedTemplatePath }), /revision marker/);
  assert.equal(fs.existsSync(path.join(outputDirectory, 'index.html')), false);
});

test('requires an existing empty output directory', t => {
  const outputDirectory = temporaryDirectory();
  const missingDirectory = path.join(outputDirectory, 'missing');
  t.after(() => removeDirectory(outputDirectory));
  expectFailure(() => generate({ outputDir: missingDirectory, revision: REVISION }), /does not exist/);

  fs.writeFileSync(path.join(outputDirectory, 'index.html'), 'existing');
  expectFailure(() => generate({ outputDir: outputDirectory, revision: REVISION }), /must not already contain index.html/);
});

test('repository metadata and template remain valid inputs', () => {
  assert.doesNotThrow(() => validateMetadata(metadata, REPOSITORY_ROOT));
  const template = fs.readFileSync(DEFAULT_TEMPLATE_PATH, 'utf8');
  assert.doesNotThrow(() => renderPage(template, metadata, REVISION));
});
