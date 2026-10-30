// What has to pass before a push. There is no CI service: run it by hand with
// `pnpm check`, or let the pre-push hook do it (see CONTRIBUTING.md).

import { spawnSync } from 'node:child_process'

const manifests = ['crates/monitor-core/Cargo.toml', 'src-tauri/Cargo.toml']
const steps = [
  // Type-checks the interface, and gives Tauri the `dist` it embeds.
  'pnpm build',
  ...manifests.map(m => `cargo clippy --locked --manifest-path ${m} --all-targets -- -D warnings`),
  ...manifests.map(m => `cargo test --locked --manifest-path ${m}`),
]

for (const step of steps) {
  console.log(`\n> ${step}`)
  const { status } = spawnSync(step, { shell: true, stdio: 'inherit' })
  if (status !== 0) process.exit(status ?? 1)
}
console.log('\nAll checks passed.')
