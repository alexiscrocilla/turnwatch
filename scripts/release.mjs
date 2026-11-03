// Releases the version in tauri.conf.json, with no CI service: builds the
// installer from a fresh clone of main, signs it for the updater and opens a
// draft release. Nothing is public until someone publishes the draft, and
// publishing it is what updates the installed apps: they read latest.json from
// the latest published release.
//
// Each system builds its own files: run it on Windows and on a Mac, in either
// order. The first run opens the draft; the second adds its files to that
// draft and its entry to the draft's latest.json. A published release is never
// touched.
//
// A clone, because what ships must be what GitHub holds, not what lies around
// in this working copy, and because a monitor running from src-tauri\target
// would lock the build. In a short folder, because the linker fails on long
// paths. The target folder stays there, so the next release builds faster.

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'

const MAC = process.platform === 'darwin'
const WORK = MAC
  ? join(homedir(), 'Library', 'Caches', 'turnwatch-release')
  : join(process.env.LOCALAPPDATA, 'turnwatch-release')
const CLONE = join(WORK, 'clone')
const TARGET = join(WORK, 'target')
// Files of the draft fetched by a second run, to merge with this one's.
const FETCHED = join(WORK, 'fetched')
// The updater's private key, which never goes in the repository.
const KEY = join(homedir(), '.tauri', 'turnwatch-updater.key')

function fail(message) {
  console.error(`\n${message}`)
  process.exit(1)
}

function run(command, options = {}) {
  console.log(`\n> ${command}`)
  const { status } = spawnSync(command, { shell: true, stdio: 'inherit', ...options })
  if (status !== 0) fail(`Failed: ${command}`)
}

// The command's output, or null when it fails.
function read(command) {
  const { status, stdout } = spawnSync(command, { shell: true, encoding: 'utf8' })
  return status === 0 ? stdout.trim() : null
}

const { version, plugins } = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8'))
const { pubkey, endpoints } = plugins.updater
const repository = endpoints[0].split('/releases/')[0]
const tag = `v${version}`
// The updater's name for this system, and the file it downloads.
const arch = process.arch === 'arm64' ? 'aarch64' : 'x86_64'
const platform = MAC ? `darwin-${arch}` : 'windows-x86_64'
const update = MAC ? `Turnwatch_${version}_${arch}.app.tar.gz` : `Turnwatch_${version}_x64-setup.exe`

if (read('git branch --show-current') !== 'main') fail('Release from main.')
if (read('git status --porcelain') !== '') fail('Commit your changes first.')
run('git fetch --quiet origin main')
const commit = read('git rev-parse HEAD')
if (commit !== read('git rev-parse origin/main')) fail('main and origin/main differ: push or pull first.')
const existing = read(`gh release view ${tag} --json isDraft,assets`)
const draft = existing === null ? null : JSON.parse(existing)
if (draft && !draft.isDraft) fail(`${tag} is already published.`)
if (draft && !draft.assets.some(asset => asset.name === 'latest.json')) fail(`The draft of ${tag} has no latest.json.`)
if (draft?.assets.some(asset => asset.name === update)) fail(`The draft of ${tag} already has ${update}.`)
const tagged = read(`git ls-remote origin refs/tags/${tag}`)
if (tagged && !tagged.startsWith(commit)) fail(`${tag} already points at another commit.`)
if (!existsSync(KEY)) fail(`No updater key at ${KEY}: see CONTRIBUTING.md.`)
if (!process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD) fail('Set TAURI_SIGNING_PRIVATE_KEY_PASSWORD to the key\'s password.')
// Installed apps would refuse the update, and say nothing.
if (readFileSync(`${KEY}.pub`, 'utf8').trim() !== pubkey) fail(`${KEY} does not match the public key in tauri.conf.json.`)

mkdirSync(WORK, { recursive: true })
rmSync(CLONE, { recursive: true, force: true })
run(`git clone --quiet "${read('git remote get-url origin')}" "${CLONE}"`)
const inClone = { cwd: CLONE, env: { ...process.env, CARGO_TARGET_DIR: TARGET } }
run(`git checkout --quiet --detach ${commit}`, inClone)
run('pnpm install --frozen-lockfile', inClone)
run('pnpm check', inClone)
run('pnpm tauri build', inClone)

// What this system ships: the file the updater downloads, and on macOS the
// disk image people install from.
let bundle
let files
if (MAC) {
  bundle = join(TARGET, 'release', 'bundle', 'macos')
  // The updater's archive holds the bundle itself, as Tauri makes it, and
  // nothing else: AppleDouble `._` files would land in the updated bundle and
  // break its signature.
  const plainTar = { cwd: bundle, env: { ...process.env, COPYFILE_DISABLE: '1' } }
  run(`tar --no-mac-metadata --no-xattrs -czf "${update}" Turnwatch.app`, plainTar)
  const image = `Turnwatch_${version}_${arch}.dmg`
  run(`cp "${join(TARGET, 'release', 'bundle', 'dmg', image)}" "${join(bundle, image)}"`)
  files = [update, image]
} else {
  bundle = join(TARGET, 'release', 'bundle', 'nsis')
  files = [update]
}
const signed = join(bundle, update)
const sums = join(bundle, 'SHA256SUMS.txt')
const manifest = join(bundle, 'latest.json')

run(`pnpm tauri signer sign --private-key-path "${KEY}" --app-version ${version} "${signed}"`, inClone)
const entry = {
  signature: readFileSync(`${signed}.sig`, 'utf8').trim(),
  url: `${repository}/releases/download/${tag}/${update}`,
}

// A second run starts from the draft's latest.json and SHA256SUMS.txt.
let platforms = {}
let sumLines = ''
if (draft) {
  rmSync(FETCHED, { recursive: true, force: true })
  run(`gh release download ${tag} --pattern latest.json --pattern SHA256SUMS.txt --dir "${FETCHED}"`)
  platforms = JSON.parse(readFileSync(join(FETCHED, 'latest.json'), 'utf8')).platforms
  if (existsSync(join(FETCHED, 'SHA256SUMS.txt'))) sumLines = readFileSync(join(FETCHED, 'SHA256SUMS.txt'), 'utf8')
}
for (const file of files) {
  const sha256 = createHash('sha256').update(readFileSync(join(bundle, file))).digest('hex')
  sumLines += `${sha256}  ${file}\n`
}
writeFileSync(sums, sumLines)
writeFileSync(manifest, JSON.stringify({
  version,
  pub_date: new Date().toISOString(),
  platforms: { ...platforms, [platform]: entry },
}, null, 2))

const assets = [...files.map(file => join(bundle, file)), sums, manifest].map(path => `"${path}"`).join(' ')
if (draft) {
  run(`gh release upload ${tag} ${assets} --clobber`)
} else {
  if (!tagged) {
    run(`git tag ${tag} ${commit}`)
    // The clone just passed the checks: the pre-push hook would repeat them.
    run(`git push --quiet --no-verify origin ${tag}`)
  }
  run(`gh release create ${tag} ${assets} --draft --verify-tag --generate-notes --title ${tag}`)
}
rmSync(CLONE, { recursive: true, force: true })
const other = MAC ? 'Windows' : 'a Mac'
const missing = Object.keys(platforms).length === 0 ? ` Run it on ${other} too, before publishing.` : ''
console.log(`\n${tag} is a draft.${missing} Publishing it on GitHub updates the installed apps.`)
