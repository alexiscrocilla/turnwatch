import type { AgentInfo, EngineInterface, Register, SessionRateLimit } from 'claude-code'

// What this mod writes for Turnwatch, in `<config>/session-monitor`:
//   quota.json            the plan's quota, the same for every session: the last
//                         one to get an answer from the API writes it;
//   sessions/<id>.json    the running sub-agents of session <id>.
// The monitor checks `version`: any change of format raises it.
const VERSION = 1

// A sub-agent that still counts: `idle` is a teammate waiting for a message.
const ACTIVE: readonly AgentInfo['status'][] = ['pending', 'running', 'waiting']

// The last content written for this session: nothing to write if it is the same.
let written = ''

async function folder($: EngineInterface) {
  const config = await $.env.get('CLAUDE_CONFIG_DIR')
  if (config) return `${config}/session-monitor`
  const home = (await $.env.get('USERPROFILE')) ?? (await $.env.get('HOME'))
  if (home === undefined) throw new Error('home folder unknown')
  return `${home}/.claude/session-monitor`
}

// `resetsAt` is an ISO date, the monitor counts in milliseconds.
function window(limits: SessionRateLimit[], kind: string) {
  const limit = limits.find(l => l.kind === kind)
  if (limit === undefined) return undefined

  const resetsAt = Date.parse(limit.resetsAt ?? '')
  return {
    percent: limit.percentUsed,
    ...(Number.isNaN(resetsAt) ? {} : { resetsAt }),
  }
}

// Like the write functions after it, it never throws: the monitor stays on
// Desktop if this mod writes nothing, and a failure must not disturb the
// session being observed. The next event writes everything again.
async function writeQuota($: EngineInterface, limits: SessionRateLimit[]) {
  try {
    const fiveHour = window(limits, 'five_hour')
    const sevenDay = window(limits, 'seven_day')
    if (fiveHour === undefined && sevenDay === undefined) return

    const sampledAt = await $.clock.now()
    const quota = { version: VERSION, sampledAt, fiveHour, sevenDay }
    await $.fs.write(`${await folder($)}/quota.json`, JSON.stringify(quota))
  } catch {
    return
  }
}

async function writeAgents($: EngineInterface, sessionId: string, list: AgentInfo[]) {
  try {
    const agents = list.map(({ id, description, type }) => ({ id, description, type }))
    const text = JSON.stringify({ version: VERSION, agents })
    if (text === written) return

    await $.fs.write(`${await folder($)}/sessions/${sessionId}.json`, text)
    written = text
  } catch {
    return
  }
}

// `finished`: an agent whose end just arrived, perhaps not yet marked in the list.
async function refreshAgents($: EngineInterface, finished?: string) {
  try {
    const list = await $.agent.list()
    const active = list.filter(a => ACTIVE.includes(a.status) && a.id !== finished)
    await writeAgents($, await $.session.id(), active)
  } catch {
    return
  }
}

// A resumed session starts from nothing: its previous process may have died
// with sub-agents still on record.
async function startClean($: EngineInterface) {
  try {
    const sessionId = await $.session.id()
    if (await $.fs.exists(`${await folder($)}/sessions/${sessionId}.json`)) {
      await writeAgents($, sessionId, [])
    }
  } catch {
    return
  }
}

export const register: Register = on => {
  on('session.measure', async ($, e, next) => {
    const measure = await next(e)
    if (e.changed.includes('rateLimits')) await writeQuota($, e.rateLimits)
    return measure
  })

  on('session.start', async ($, e, next) => {
    const started = await next(e)
    await startClean($)
    return started
  })

  on('agent.spawn', async ($, e, next) => {
    const spawned = await next(e)
    await refreshAgents($)
    return spawned
  })

  on('turn.complete', async ($, e, next) => {
    const completed = await next(e)
    await refreshAgents($, e.agentId)
    return completed
  })

  on('session.end', async ($, e, next) => {
    await writeAgents($, e.sessionId, [])
    return next(e)
  })
}
