const {createHash} = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const [versionArg, output] = process.argv.slice(2);
const version = Number(versionArg);
if (!Number.isSafeInteger(version) || version < 1 || !output) {
  throw new Error('Usage: node scripts/prepare_guide_release.cjs VERSION NEW_OUTPUT_DIRECTORY');
}

const project = path.resolve(__dirname, '..');
const gifDir = 'docs/assets/tutorials';
const files = ['README.md', 'README.zh-CN.md', 'docs/tutorials.md', 'docs/self-update.md',
  ...fs.readdirSync(path.join(project, gifDir)).filter(name => name.endsWith('.gif')).map(name => `${gifDir}/${name}`)];
let copy;
vm.runInNewContext(fs.readFileSync(path.join(project, 'crates/emp-app/web/copy-tutorials.js'), 'utf8'), {
  registerUiCopy: table => { copy = table; }
});
const copyKeys = ['gpt', 'external', 'shared', 'team', 'native_context', 'native_name', 'remove_subscription',
  'add', 'context', 'rename', 'remove', 'labels', 'export', 'import', 'tunnel', 'team_overview', 'team_share',
  'team_approve', 'team_members'];
const pack = {schema_version: 1, team_join: {}, titles: {}};
for (const [index, locale] of ['zh', 'en'].entries()) {
  const text = key => copy[`tutorials.${key}`][index];
  pack.team_join[locale] = {
    title: text('team_join'),
    steps: ['team_join_install', 'team_join_invitation', 'team_join_submit'].map(text)
  };
  pack.titles[locale] = Object.fromEntries(copyKeys.map(key => [key, text(key)]));
}

const manifest = {schema_version: 1, version, files: {}};
fs.mkdirSync(output);
for (const filename of [...files, 'tutorials.json'].sort()) {
  const body = filename === 'tutorials.json' ? Buffer.from(JSON.stringify(pack, null, 2) + '\n')
    : fs.readFileSync(path.join(project, filename));
  const size = body.length;
  const limit = filename.endsWith('.gif') ? 8 * 1024 * 1024 : filename.endsWith('.md') ? 512 * 1024 : 32 * 1024;
  if (size === 0 || size > limit) throw new Error(`${filename} must contain 1–${limit} bytes`);
  const target = path.join(output, 'content', filename);
  fs.mkdirSync(path.dirname(target), {recursive: true});
  fs.writeFileSync(target, body);
  manifest.files[filename] = {sha256: createHash('sha256').update(body).digest('hex'), size};
}
fs.writeFileSync(path.join(output, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
