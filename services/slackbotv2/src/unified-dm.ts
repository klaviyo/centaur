import { AsyncLocalStorage } from 'node:async_hooks'
import type { Lock, Message, StateAdapter } from 'chat'
import type { SlackbotV2Options } from './types'
import { isJsonObject, stringValue } from './utils'

const USER_ENTRY = /^T[A-Z0-9]+:[UW][A-Z0-9]+$/
const CANONICAL_DM = /^slack:D[A-Z0-9]+:$/
type OwnedLease = {
  active: boolean
  leaseMs: number
  lock: Lock
  lost: boolean
  state: StateAdapter
  threadId: string
}
const ownedLease = new AsyncLocalStorage<OwnedLease>()

/** A malformed deployment allowlist must never broaden access. */
export function parseUnifiedDmUsers(value: string | undefined): string[] {
  const entries = (value ?? '').split(/[\s,]+/).filter(Boolean)
  if (entries.some(entry => !USER_ENTRY.test(entry))) {
    throw new Error('SLACKBOTV2_UNIFIED_DM_USERS must contain team:user pairs')
  }
  return [...new Set(entries)]
}

/** Called only after the signed-webhook and message policy gates. */
export function unifiedDmThreadId(
  message: Message,
  options: SlackbotV2Options
): string | undefined {
  if (message.author.isBot === true || message.author.isMe) return undefined
  const raw = isJsonObject(message.raw) ? message.raw : {}
  if (raw.user !== message.author.userId) return undefined
  return unifiedDmThreadIdForEvent(raw, options)
}

/** Also used before SDK dispatch, with the verified envelope's team_id. */
export function unifiedDmThreadIdForEvent(
  event: unknown,
  options: SlackbotV2Options,
  envelopeTeamId?: unknown
): string | undefined {
  if (options.unifiedDmEnabled !== true || !options.slackHomeTeamId) return undefined
  const raw = isJsonObject(event) ? event : {}
  const channel = stringValue(raw.channel)
  const user = stringValue(raw.user)
  const team = stringValue(envelopeTeamId) ?? stringValue(raw.team_id) ?? stringValue(raw.team)
  if (
    (raw.type !== 'message' && raw.type !== 'app_mention') ||
    raw.channel_type !== 'im' || !channel || !/^D[A-Z0-9]+$/.test(channel) ||
    !user || !/^[UW][A-Z0-9]+$/.test(user) || user === options.botUserId ||
    team !== options.slackHomeTeamId || raw.bot_id || raw.bot_profile ||
    raw.subtype === 'bot_message'
  ) return undefined
  // Slack Connect sender identity must agree with the authenticated workspace.
  for (const candidate of [raw.team, raw.team_id, raw.user_team, raw.source_team]) {
    if (candidate !== undefined && candidate !== team) return undefined
  }
  const owner = `${team}:${user}`
  if (!USER_ENTRY.test(owner) || !options.unifiedDmUsers?.includes(owner)) return undefined
  // Reuse the SDK's existing conversation-wide DM identity. No old roots are
  // rewritten or merged; disabling the flag leaves their sessions intact.
  return `slack:${channel}:`
}

export function isConversationDmThread(threadId: string): boolean {
  return CANONICAL_DM.test(threadId)
}

export class UnifiedDmLockError extends Error {
  constructor(readonly ownershipLost = false) {
    super('The continuous DM handoff is busy; retry required')
    this.name = 'UnifiedDmLockError'
  }
}

/** Stop a stale holder before its next state/API mutation, after any await. */
export async function assertUnifiedDmLockOwned(threadId: string): Promise<void> {
  const lease = ownedLease.getStore()
  if (!lease || lease.threadId !== threadId) return
  if (!lease.active || lease.lost) throw new UnifiedDmLockError(true)
  try {
    if (!(await lease.state.extendLock(lease.lock, lease.leaseMs))) lease.lost = true
  } catch {
    lease.lost = true
  }
  if (lease.lost) throw new UnifiedDmLockError(true)
}

/**
 * SDK callback locks are keyed by the original Slack root and may be force
 * released. Use a separate, token-owned state-adapter lock for the canonical
 * handoff, including retries and render state transitions. Production uses
 * the existing Postgres lock table; there is no process-local mutex or schema.
 * Blank-ts legacy DMs use this too so in-flight work survives a flag rollback.
 */
export async function withUnifiedDmLock<T>(
  state: StateAdapter,
  threadId: string,
  work: () => Promise<T>,
  timing: { waitMs?: number; leaseMs?: number; pollMs?: number; refreshMs?: number } = {}
): Promise<T> {
  if (!isConversationDmThread(threadId)) return work()
  const leaseMs = timing.leaseMs ?? 120_000
  const deadline = Date.now() + (timing.waitMs ?? 30_000)
  const key = `slackbotv2:unified-dm:handoff:${threadId}`
  let lock = await state.acquireLock(key, leaseMs)
  while (!lock) {
    if (Date.now() >= deadline) throw new UnifiedDmLockError()
    await new Promise(resolve => setTimeout(resolve, timing.pollMs ?? 25))
    lock = await state.acquireLock(key, leaseMs)
  }
  const ownedLock = lock
  const lease: OwnedLease = {
    active: true, leaseMs, lock: ownedLock, lost: false, state, threadId
  }
  let renewal = Promise.resolve()
  const timer = setInterval(() => {
    renewal = renewal.then(async () => {
      if (lease.lost || !lease.active) return
      try {
        if (!(await state.extendLock(ownedLock, leaseMs))) lease.lost = true
      } catch {
        lease.lost = true
      }
    })
  }, timing.refreshMs ?? 20_000)
  try {
    const result = await ownedLease.run(lease, work)
    await renewal
    if (lease.lost) throw new UnifiedDmLockError(true)
    return result
  } finally {
    lease.active = false
    clearInterval(timer)
    await renewal
    // releaseLock compares the ownership token atomically. An expired holder
    // must never release a newer worker's lease.
    await state.releaseLock(ownedLock)
  }
}
