import type { AgentInfo, SessionMeasureInput } from 'claude-code'
import { expect, test } from 'claude-code/testing'

const DOSSIER = 'C:/Users/moi/.claude/session-monitor'

const agent = (id: string, status: AgentInfo['status']): AgentInfo => ({
  id,
  description: `Tâche ${id}`,
  type: 'Explore',
  status,
})

type Bas = {
  fichiers: Map<string, string>
  agents: AgentInfo[]
}

// Rien ne se trouve sous le mod : ces hooks tiennent lieu du moteur.
function moteur(on: Parameters<Parameters<typeof test>[1]>[1], echec = false): Bas {
  const bas: Bas = { fichiers: new Map(), agents: [] }
  on('env.get', (_, e) => ({ value: e.name === 'USERPROFILE' ? 'C:/Users/moi' : undefined }))
  on('clock.now', () => ({ value: 1_000 }))
  on('session.id', () => ({ value: 'session-1' }))
  on('agent.list', () => ({ value: bas.agents }))
  on('fs.exists', (_, e) => ({ value: bas.fichiers.has(slashes(e.path)) }))
  on('fs.write', (_, e) => {
    if (echec) throw new Error('disque plein')
    bas.fichiers.set(slashes(e.path), e.text)
    return { value: undefined }
  })
  return bas
}

// Le moteur remet les chemins au format du système.
const slashes = (chemin: string) => chemin.replaceAll('\\', '/')

const mesure = (patch: Partial<SessionMeasureInput>) =>
  ({
    context: {},
    rateLimits: [],
    changed: ['rateLimits'],
    ...patch,
  }) as SessionMeasureInput

test('le quota est écrit avec ses heures de réinitialisation en millisecondes', async ($, on) => {
  const bas = moteur(on)
  on('session.measure', (_, e) => ({ changed: e.changed }))

  await $.session.measure(
    mesure({
      rateLimits: [
        { kind: 'five_hour', percentUsed: 23.5, resetsAt: '2026-10-30T18:00:00.000Z' },
        { kind: 'seven_day', percentUsed: 11 },
        { kind: 'spend_limit', percentUsed: 3 },
      ],
    }),
  )

  expect(JSON.parse(bas.fichiers.get(`${DOSSIER}/quota.json`) ?? 'null')).toEqual({
    version: 1,
    sampledAt: 1_000,
    fiveHour: { percent: 23.5, resetsAt: Date.parse('2026-10-30T18:00:00.000Z') },
    sevenDay: { percent: 11 },
  })
})

test("une mesure sans quota n'écrit rien", async ($, on) => {
  const bas = moteur(on)
  on('session.measure', (_, e) => ({ changed: e.changed }))

  await $.session.measure(mesure({ changed: ['cost'] }))
  await $.session.measure(mesure({ rateLimits: [{ kind: 'spend_limit', percentUsed: 3 }] }))

  expect(bas.fichiers.size).toBe(0)
})

test('les sous-agents actifs sont écrits, et retirés dès leur fin', async ($, on) => {
  const bas = moteur(on)
  on('agent.spawn', () => ({ model: 'claude-haiku-5-5', agentId: 'a1' }))
  on('turn.complete', () => ({ text: '' }))
  const chemin = `${DOSSIER}/sessions/session-1.json`
  const lus = () => JSON.parse(bas.fichiers.get(chemin) ?? 'null')

  bas.agents = [agent('a1', 'running'), agent('a0', 'completed')]
  await $.agent.spawn({ prompt: 'Cherche', description: 'Tâche a1' })
  expect(lus()).toEqual({
    version: 1,
    agents: [{ id: 'a1', description: 'Tâche a1', type: 'Explore' }],
  })

  // La fin du sous-agent arrive alors que la liste le dit encore en cours.
  await $.turn.complete({ agentId: 'a1', reason: 'answer', answer: 'fini' } as never)
  expect(lus()).toEqual({ version: 1, agents: [] })
})

test('une session reprise repart sans sous-agents', async ($, on) => {
  const bas = moteur(on)
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  const chemin = `${DOSSIER}/sessions/session-1.json`
  bas.fichiers.set(chemin, '{"version":1,"agents":[{"id":"mort"}]}')

  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })

  expect(JSON.parse(bas.fichiers.get(chemin) ?? 'null')).toEqual({ version: 1, agents: [] })
})

test("une écriture qui échoue ne gêne pas la session", async ($, on) => {
  moteur(on, true)
  on('session.measure', (_, e) => ({ changed: e.changed }))

  const fin = await $.session.measure(
    mesure({ rateLimits: [{ kind: 'five_hour', percentUsed: 1 }] }),
  )

  expect(fin).toEqual({ changed: ['rateLimits'] })
})
