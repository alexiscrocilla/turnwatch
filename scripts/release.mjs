// Releases the version in tauri.conf.json, with no CI service: builds the
// installer from a fresh clone of main and opens a draft release. Nothing is
// public until someone publishes the draft.
//
// A clone, because what ships must be what GitHub holds, not what lies around
// in this working copy, and because a monitor running from src-tauri\target
// would lock the build. In a short folder, because the linker fails on long
// paths. The target folder stays there, so the next release builds faster.

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

const WORK = join(process.env.LOCALAPPDATA, 'csm-release')
const CLONE = join(WORK, 'clone')
const TARGET = join(WORK, 'target')

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

const { version } = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8'))
const tag = `v${version}`

if (read('git branch --show-current') !== 'main') fail('Release from main.')
if (read('git status --porcelain') !== '') fail('Commit your changes first.')
run('git fetch --quiet origin main')
const commit = read('git rev-parse HEAD')
if (commit !== read('git rev-parse origin/main')) fail('main and origin/main differ: push or pull first.')
if (read(`gh release view ${tag} --json tagName`) !== null) fail(`${tag} already has a release.`)
const tagged = read(`git ls-remote origin refs/tags/${tag}`)
if (tagged && !tagged.startsWith(commit)) fail(`${tag} already points at another commit.`)

mkdirSync(WORK, { recursive: true })
rmSync(CLONE, { recursive: true, force: true })
run(`git clone --quiet "${read('git remote get-url origin')}" "${CLONE}"`)
const inClone = { cwd: CLONE, env: { ...process.env, CARGO_TARGET_DIR: TARGET } }
run(`git checkout --quiet --detach ${commit}`, inClone)
run('pnpm install --frozen-lockfile', inClone)
run('pnpm check', inClone)
run('pnpm tauri build', inClone)

// GitHub turns the spaces of an asset's name into dots: name it first, so that
// the file people download is the one the checksum names.
const bundle = join(TARGET, 'release', 'bundle', 'nsis')
const installer = `Claude-Session-Monitor_${version}_x64-setup.exe`
renameSync(join(bundle, `Claude Session Monitor_${version}_x64-setup.exe`), join(bundle, installer))
const sha256 = createHash('sha256').update(readFileSync(join(bundle, installer))).digest('hex')
const sums = join(bundle, 'SHA256SUMS.txt')
writeFileSync(sums, `${sha256}  ${installer}\n`)

if (!tagged) {
  run(`git tag ${tag} ${commit}`)
  // The clone just passed the checks: the pre-push hook would repeat them.
  run(`git push --quiet --no-verify origin ${tag}`)
}
run(`gh release create ${tag} "${join(bundle, installer)}" "${sums}" --draft --verify-tag --generate-notes --title ${tag}`)
rmSync(CLONE, { recursive: true, force: true })
console.log(`\n${tag} is a draft. Publish it on GitHub when it is ready.`)
