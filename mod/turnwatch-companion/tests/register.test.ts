import type { AgentInfo, SessionMeasureInput } from 'claude-code'
import { expect, test } from 'claude-code/testing'

const FOLDER = 'C:/Users/me/.claude/session-monitor'

const agent = (id: string, status: AgentInfo['status']): AgentInfo => ({
  id,
  description: `Task ${id}`,
  type: 'Explore',
  status,
})

type Below = {
  files: Map<string, string>
  agents: AgentInfo[]
}

// Nothing sits under the mod: these hooks stand in for the engine.
function engine(on: Parameters<Parameters<typeof test>[1]>[1], failing = false): Below {
  const below: Below = { files: new Map(), agents: [] }
  on('env.get', (_, e) => ({ value: e.name === 'USERPROFILE' ? 'C:/Users/me' : undefined }))
  on('clock.now', () => ({ value: 1_000 }))
  on('session.id', () => ({ value: 'session-1' }))
  on('agent.list', () => ({ value: below.agents }))
  on('fs.exists', (_, e) => ({ value: below.files.has(slashes(e.path)) }))
  on('fs.write', (_, e) => {
    if (failing) throw new Error('disk full')
    below.files.set(slashes(e.path), e.text)
    return { value: undefined }
  })
  return below
}

// The engine puts paths back in the system's format.
const slashes = (path: string) => path.replaceAll('\\', '/')

const measure = (patch: Partial<SessionMeasureInput>) =>
  ({
    context: {},
    rateLimits: [],
    changed: ['rateLimits'],
    ...patch,
  }) as SessionMeasureInput

test('the quota is written with its reset times in milliseconds', async ($, on) => {
  const below = engine(on)
  on('session.measure', (_, e) => ({ changed: e.changed }))

  await $.session.measure(
    measure({
      rateLimits: [
        { kind: 'five_hour', percentUsed: 23.5, resetsAt: '2026-10-30T18:00:00.000Z' },
        { kind: 'seven_day', percentUsed: 11 },
        { kind: 'spend_limit', percentUsed: 3 },
      ],
    }),
  )

  expect(JSON.parse(below.files.get(`${FOLDER}/quota.json`) ?? 'null')).toEqual({
    version: 1,
    sampledAt: 1_000,
    fiveHour: { percent: 23.5, resetsAt: Date.parse('2026-10-30T18:00:00.000Z') },
    sevenDay: { percent: 11 },
  })
})

test('a measure without a quota writes nothing', async ($, on) => {
  const below = engine(on)
  on('session.measure', (_, e) => ({ changed: e.changed }))

  await $.session.measure(measure({ changed: ['cost'] }))
  await $.session.measure(measure({ rateLimits: [{ kind: 'spend_limit', percentUsed: 3 }] }))

  expect(below.files.size).toBe(0)
})

test('active sub-agents are written, and removed as soon as they end', async ($, on) => {
  const below = engine(on)
  on('agent.spawn', () => ({ model: 'claude-haiku-5-5', agentId: 'a1' }))
  on('turn.complete', () => ({ text: '' }))
  const path = `${FOLDER}/sessions/session-1.json`
  const read = () => JSON.parse(below.files.get(path) ?? 'null')

  below.agents = [agent('a1', 'running'), agent('a0', 'completed')]
  await $.agent.spawn({ prompt: 'Search', description: 'Task a1' })
  expect(read()).toEqual({
    version: 1,
    agents: [{ id: 'a1', description: 'Task a1', type: 'Explore' }],
  })

  // The sub-agent's end arrives while the list still says it is running.
  await $.turn.complete({ agentId: 'a1', reason: 'answer', answer: 'done' } as never)
  expect(read()).toEqual({ version: 1, agents: [] })
})

test('a session without sub-agents leaves no file', async ($, on) => {
  const below = engine(on)
  on('turn.complete', () => ({ text: '' }))
  on('session.end', (_, e) => ({ sessionId: e.sessionId }))

  await $.turn.complete({ reason: 'answer', answer: 'done' } as never)
  await $.session.end({ reason: 'other', sessionId: 'session-1' } as never)

  expect(below.files.size).toBe(0)
})

test('a resumed session starts without sub-agents', async ($, on) => {
  const below = engine(on)
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  const path = `${FOLDER}/sessions/session-1.json`
  below.files.set(path, '{"version":1,"agents":[{"id":"dead"}]}')

  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })

  expect(JSON.parse(below.files.get(path) ?? 'null')).toEqual({ version: 1, agents: [] })
})

test('a failed write does not disturb the session', async ($, on) => {
  engine(on, true)
  on('session.measure', (_, e) => ({ changed: e.changed }))

  const result = await $.session.measure(
    measure({ rateLimits: [{ kind: 'five_hour', percentUsed: 1 }] }),
  )

  expect(result).toEqual({ changed: ['rateLimits'] })
})
