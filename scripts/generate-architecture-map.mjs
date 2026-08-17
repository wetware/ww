#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
export const REPOSITORY_ROOT = path.resolve(SCRIPT_DIR, '..');
export const DEFAULT_METADATA_PATH = path.join(REPOSITORY_ROOT, 'diagrams', 'architecture-map.json');
export const DEFAULT_TEMPLATE_PATH = path.join(REPOSITORY_ROOT, 'diagrams', 'architecture-map.template.html');
const DATA_MARKER = '__ARCHITECTURE_MAP_DATA__';
const REVISION_MARKER = '__ARCHITECTURE_MAP_REVISION__';

function fail(message) {
  throw new Error(`Architecture map: ${message}`);
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function requireString(value, name) {
  if (typeof value !== 'string' || value.length === 0) fail(`${name} must be a non-empty string`);
}

function requireFinite(value, name, allowZero = true) {
  if (!Number.isFinite(value) || (allowZero ? value < 0 : value <= 0)) {
    fail(`${name} must be a ${allowZero ? 'non-negative' : 'positive'} finite number`);
  }
}

export function parseArguments(argv) {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const flag = argv[index];
    const value = argv[index + 1];
    if ((flag !== '--output' && flag !== '--revision') || value === undefined || values.has(flag)) {
      fail('usage: node scripts/generate-architecture-map.mjs --output <directory> --revision <40-character lowercase SHA>');
    }
    values.set(flag, value);
  }
  if (values.size !== 2) {
    fail('usage: node scripts/generate-architecture-map.mjs --output <directory> --revision <40-character lowercase SHA>');
  }
  const revision = values.get('--revision');
  if (!/^[0-9a-f]{40}$/.test(revision)) fail('--revision must be a 40-character lowercase Git SHA');
  return { outputDir: values.get('--output'), revision };
}

export function validateSourcePath(sourcePath, rootDir = REPOSITORY_ROOT) {
  requireString(sourcePath, 'source path');
  if (path.isAbsolute(sourcePath)) fail(`source path must be relative: ${sourcePath}`);
  const resolved = path.resolve(rootDir, sourcePath);
  const relative = path.relative(rootDir, resolved);
  if (relative === '' || relative === '..' || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    fail(`source path escapes the repository: ${sourcePath}`);
  }
  let stat;
  try {
    stat = fs.lstatSync(resolved);
  } catch {
    fail(`referenced source file does not exist: ${sourcePath}`);
  }
  if (stat.isSymbolicLink()) fail(`source path must not be a symbolic link: ${sourcePath}`);
  if (!stat.isFile()) fail(`source path must be a regular file: ${sourcePath}`);
}

export function validateMetadata(metadata, rootDir = REPOSITORY_ROOT) {
  if (!isRecord(metadata)) fail('metadata root must be an object');
  if (metadata.schemaVersion !== 1) fail('schemaVersion must equal 1');
  if (!Array.isArray(metadata.nodes) || metadata.nodes.length === 0) fail('nodes must be a non-empty array');
  if (!Array.isArray(metadata.routes)) fail('routes must be an array');

  const keys = new Set();
  for (const [index, node] of metadata.nodes.entries()) {
    if (!isRecord(node)) fail(`nodes[${index}] must be an object`);
    for (const field of ['key', 'id', 'label', 'group', 'title', 'subtitle', 'what', 'built']) requireString(node[field], `nodes[${index}].${field}`);
    if (keys.has(node.key)) fail(`node key is duplicated: ${node.key}`);
    keys.add(node.key);
    for (const field of ['x', 'y', 'w', 'h', 'z']) requireFinite(node[field], `nodes[${index}].${field}`, field === 'x' || field === 'y' || field === 'z');
    if (!Array.isArray(node.files) || node.files.length === 0) fail(`nodes[${index}].files must be a non-empty array`);
    for (const sourcePath of node.files) validateSourcePath(sourcePath, rootDir);
  }

  for (const [index, route] of metadata.routes.entries()) {
    if (!isRecord(route)) fail(`routes[${index}] must be an object`);
    requireString(route.from, `routes[${index}].from`);
    requireString(route.to, `routes[${index}].to`);
    if (!keys.has(route.from) || !keys.has(route.to)) fail(`routes[${index}] references an unknown node`);
    if (route.style !== 'primary' && route.style !== 'secondary') fail(`routes[${index}].style must be primary or secondary`);
  }
  return metadata;
}

function readJson(metadataPath) {
  try {
    return JSON.parse(fs.readFileSync(metadataPath, 'utf8'));
  } catch (error) {
    fail(`cannot parse metadata ${metadataPath}: ${error.message}`);
  }
}

function exactlyOneMarker(template, marker, name) {
  const count = template.split(marker).length - 1;
  if (count !== 1) fail(`template must contain exactly one ${name} marker`);
}

export function serializeForScript(metadata) {
  return JSON.stringify(metadata).replace(/[<>&\u2028\u2029]/g, character => ({
    '<': '\\u003c',
    '>': '\\u003e',
    '&': '\\u0026',
    '\u2028': '\\u2028',
    '\u2029': '\\u2029'
  })[character]);
}

export function renderPage(template, metadata, revision) {
  exactlyOneMarker(template, DATA_MARKER, 'data');
  exactlyOneMarker(template, REVISION_MARKER, 'revision');
  return template
    .replace(DATA_MARKER, serializeForScript(metadata))
    .replace(REVISION_MARKER, revision.slice(0, 8).toUpperCase());
}

function validateOutputDirectory(outputDir) {
  let stat;
  try {
    stat = fs.lstatSync(outputDir);
  } catch {
    fail(`output directory does not exist: ${outputDir}`);
  }
  if (stat.isSymbolicLink() || !stat.isDirectory()) fail(`output path must be a directory: ${outputDir}`);
  const outputPath = path.join(outputDir, 'index.html');
  if (fs.existsSync(outputPath)) fail(`output directory must not already contain index.html: ${outputDir}`);
  return outputPath;
}

export function generate({ outputDir, revision, rootDir = REPOSITORY_ROOT, metadataPath = DEFAULT_METADATA_PATH, templatePath = DEFAULT_TEMPLATE_PATH }) {
  if (!/^[0-9a-f]{40}$/.test(revision)) fail('--revision must be a 40-character lowercase Git SHA');
  const metadata = validateMetadata(readJson(metadataPath), rootDir);
  let template;
  try {
    template = fs.readFileSync(templatePath, 'utf8');
  } catch (error) {
    fail(`cannot read template ${templatePath}: ${error.message}`);
  }
  const page = renderPage(template, metadata, revision);
  const outputPath = validateOutputDirectory(outputDir);
  fs.writeFileSync(outputPath, page);
  return outputPath;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    generate(parseArguments(process.argv.slice(2)));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
