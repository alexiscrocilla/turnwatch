import type { AgentInfo, EngineInterface, Register, SessionRateLimit } from 'claude-code'

// Ce que ce mod écrit pour Claude Session Monitor, dans `<config>/session-monitor` :
//   quota.json            le quota du forfait, le même pour toutes les sessions :
//                         la dernière qui a reçu une réponse de l'API l'écrit ;
//   sessions/<id>.json    les sous-agents en cours de la session <id>.
// Le moniteur vérifie `version` : tout changement de format la fait monter.
const VERSION = 1

// Un sous-agent qui compte encore : `idle` est un coéquipier qui attend un message.
const ACTIFS: readonly AgentInfo['status'][] = ['pending', 'running', 'waiting']

// Le dernier contenu écrit pour cette session : rien à réécrire s'il ne change pas.
let ecrit = ''

async function dossier($: EngineInterface) {
  const config = await $.env.get('CLAUDE_CONFIG_DIR')
  if (config) return `${config}/session-monitor`
  const maison = (await $.env.get('USERPROFILE')) ?? (await $.env.get('HOME'))
  if (maison === undefined) throw new Error('dossier personnel inconnu')
  return `${maison}/.claude/session-monitor`
}

// `resetsAt` est une date ISO, le moniteur compte en millisecondes.
function fenetre(limites: SessionRateLimit[], kind: string) {
  const limite = limites.find(l => l.kind === kind)
  if (limite === undefined) return undefined

  const resetsAt = Date.parse(limite.resetsAt ?? '')
  return {
    percent: limite.percentUsed,
    ...(Number.isNaN(resetsAt) ? {} : { resetsAt }),
  }
}

// Comme les fonctions d'écriture qui suivent, elle ne lève jamais : le moniteur
// reste sur Desktop si ce mod n'écrit rien, et un échec ne doit pas gêner la
// session observée. Le prochain événement réécrit tout.
async function ecrireQuota($: EngineInterface, limites: SessionRateLimit[]) {
  try {
    const fiveHour = fenetre(limites, 'five_hour')
    const sevenDay = fenetre(limites, 'seven_day')
    if (fiveHour === undefined && sevenDay === undefined) return

    const sampledAt = await $.clock.now()
    const quota = { version: VERSION, sampledAt, fiveHour, sevenDay }
    await $.fs.write(`${await dossier($)}/quota.json`, JSON.stringify(quota))
  } catch {
    return
  }
}

async function ecrireAgents($: EngineInterface, sessionId: string, liste: AgentInfo[]) {
  try {
    const agents = liste.map(({ id, description, type }) => ({ id, description, type }))
    const texte = JSON.stringify({ version: VERSION, agents })
    if (texte === ecrit) return

    await $.fs.write(`${await dossier($)}/sessions/${sessionId}.json`, texte)
    ecrit = texte
  } catch {
    return
  }
}

// `fini` : un agent dont la fin vient d'arriver, peut-être pas encore noté dans la liste.
async function actualiserAgents($: EngineInterface, fini?: string) {
  try {
    const liste = await $.agent.list()
    const actifs = liste.filter(a => ACTIFS.includes(a.status) && a.id !== fini)
    await ecrireAgents($, await $.session.id(), actifs)
  } catch {
    return
  }
}

// Une session reprise repart de zéro : son processus précédent a pu mourir avec
// des sous-agents encore notés.
async function repartirDeZero($: EngineInterface) {
  try {
    const sessionId = await $.session.id()
    if (await $.fs.exists(`${await dossier($)}/sessions/${sessionId}.json`)) {
      await ecrireAgents($, sessionId, [])
    }
  } catch {
    return
  }
}

export const register: Register = on => {
  on('session.measure', async ($, e, next) => {
    const mesure = await next(e)
    if (e.changed.includes('rateLimits')) await ecrireQuota($, e.rateLimits)
    return mesure
  })

  on('session.start', async ($, e, next) => {
    const debut = await next(e)
    await repartirDeZero($)
    return debut
  })

  on('agent.spawn', async ($, e, next) => {
    const lance = await next(e)
    await actualiserAgents($)
    return lance
  })

  on('turn.complete', async ($, e, next) => {
    const fin = await next(e)
    await actualiserAgents($, e.agentId)
    return fin
  })

  on('session.end', async ($, e, next) => {
    await ecrireAgents($, e.sessionId, [])
    return next(e)
  })
}
