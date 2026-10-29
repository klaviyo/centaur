import { describe, expect, it } from 'bun:test'
import { createMemoryState } from '@chat-adapter/state-memory'
import { Message, parseMarkdown } from 'chat'
import type { SlackbotV2Options } from '../src/types'
import {
  assertUnifiedDmLockOwned,
  parseUnifiedDmUsers,
  unifiedDmThreadId,
  unifiedDmThreadIdForEvent,
  UnifiedDmLockError,
  withUnifiedDmLock
} from '../src/unified-dm'

const options: SlackbotV2Options = {
  apiUrl: 'http://localhost', botToken: 'test-token', signingSecret: 'test-secret',
  botUserId: 'UBOT', slackHomeTeamId: 'THOME',
  unifiedDmEnabled: true, unifiedDmUsers: ['THOME:UUSER']
}
const event = {
  type: 'message', channel: 'DPRIVATE', channel_type: 'im',
  user: 'UUSER', team_id: 'THOME', ts: '1700000000.001', text: 'Hello'
}
function message(raw: Record<string, unknown> = event): Message {
  return new Message({
    attachments: [],
    id: '1700000000.001', threadId: 'slack:DPRIVATE:1700000000.001',
    text: 'Hello', formatted: parseMarkdown('Hello'), raw,
    author: { userId: 'UUSER', userName: 'user', fullName: 'User', isBot: false, isMe: false },
    metadata: { dateSent: new Date(), edited: false }
  })
}

describe('unified DM eligibility', () => {
  it('parses and deduplicates explicit workspace/member pairs', () => {
    expect(parseUnifiedDmUsers('THOME:UUSER, THOME:UUSER\nTOTHER:WUSER'))
      .toEqual(['THOME:UUSER', 'TOTHER:WUSER'])
    expect(parseUnifiedDmUsers(undefined)).toEqual([])
  })

  it.each(['UUSER', '*', 'THOME:*', 'THOME:BUSER', 'thome:UUSER', 'THOME:UUSER:EXTRA'])(
    'rejects malformed allowlist entry %s', entry => {
      expect(() => parseUnifiedDmUsers(entry)).toThrow('must contain team:user pairs')
    }
  )

  it('maps roots and replies to the conversation identity without changing raw provenance', () => {
    const reply = message({ ...event, thread_ts: '1699999999.001' })
    expect(unifiedDmThreadId(reply, options)).toBe('slack:DPRIVATE:')
    expect(reply.threadId).toBe('slack:DPRIVATE:1700000000.001')
    expect(reply.raw).toMatchObject({ thread_ts: '1699999999.001' })
  })

  it.each([
    { unifiedDmEnabled: false }, { unifiedDmEnabled: undefined },
    { unifiedDmUsers: [] }, { unifiedDmUsers: ['TOTHER:UUSER'] },
    { slackHomeTeamId: undefined }, { slackHomeTeamId: 'TOTHER' }
  ])('requires the switch, exact allowlist and authenticated home workspace %p', changes => {
    expect(unifiedDmThreadId(message(), { ...options, ...changes })).toBeUndefined()
  })

  it.each([
    { channel: 'CPUBLIC' }, { channel: 'GGROUP', channel_type: 'mpim' },
    { channel_type: 'group' }, { channel_type: undefined },
    { user: 'UOTHER' }, { team_id: undefined }, { team_id: 'TOTHER' },
    { team: 'TOTHER' }, { user_team: 'TOTHER' }, { source_team: 'TOTHER' },
    { bot_id: 'BBOT' }, { bot_profile: {} }, { subtype: 'bot_message' },
    { type: 'channel_created' }
  ])('rejects nonprivate or inconsistent identities %p', changes => {
    expect(unifiedDmThreadId(message({ ...event, ...changes }), options)).toBeUndefined()
  })

  it('rejects bot and self authors independently of the event', () => {
    const bot = message()
    bot.author.isBot = true
    expect(unifiedDmThreadId(bot, options)).toBeUndefined()
    const self = message()
    self.author.isMe = true
    expect(unifiedDmThreadId(self, options)).toBeUndefined()
  })

  it('uses signed envelope team identity before SDK normalization', () => {
    expect(unifiedDmThreadIdForEvent({ ...event, team_id: undefined }, options, 'THOME'))
      .toBe('slack:DPRIVATE:')
    expect(unifiedDmThreadIdForEvent(event, options, 'TOTHER')).toBeUndefined()
  })
})

describe('canonical handoff lock', () => {
  it('lets background finalization wait until a competing handoff releases its lock', async () => {
    const state = createMemoryState()
    await state.connect()
    const lock = await state.acquireLock('slackbotv2:unified-dm:handoff:slack:DPRIVATE:', 1000)
    let finalized = false
    const finalization = withUnifiedDmLock(state, 'slack:DPRIVATE:', async () => {
      finalized = true
    }, { waitMs: Infinity, pollMs: 1 })
    await new Promise(resolve => setTimeout(resolve, 10))
    expect(finalized).toBe(false)
    await state.releaseLock(lock!)
    await finalization
    expect(finalized).toBe(true)
  })

  it('blocks later mutations when ownership is lost during awaited work', async () => {
    const state = createMemoryState()
    await state.connect()
    let mutated = false
    await expect(withUnifiedDmLock(state, 'slack:DPRIVATE:', async () => {
      await state.forceReleaseLock('slackbotv2:unified-dm:handoff:slack:DPRIVATE:')
      await assertUnifiedDmLockOwned('slack:DPRIVATE:')
      mutated = true
    })).rejects.toMatchObject({ ownershipLost: true })
    expect(mutated).toBe(false)
  })

  it('serializes callers that share durable adapter locks', async () => {
    const state = createMemoryState()
    await state.connect()
    const sequence: string[] = []
    await Promise.all(['first', 'second'].map(label =>
      withUnifiedDmLock(state, 'slack:DPRIVATE:', async () => {
        sequence.push(`${label}:start`)
        await new Promise(resolve => setTimeout(resolve, 10))
        sequence.push(`${label}:end`)
      }, { pollMs: 1 })
    ))
    expect(sequence).toEqual(['first:start', 'first:end', 'second:start', 'second:end'])
  })

  it('times out without invoking the competing work and releases after errors', async () => {
    const state = createMemoryState()
    await state.connect()
    const lock = await state.acquireLock('slackbotv2:unified-dm:handoff:slack:DPRIVATE:', 1000)
    let called = false
    await expect(withUnifiedDmLock(state, 'slack:DPRIVATE:', async () => {
      called = true
    }, { waitMs: 3, pollMs: 1 })).rejects.toBeInstanceOf(UnifiedDmLockError)
    expect(called).toBe(false)
    await state.releaseLock(lock!)
    await expect(withUnifiedDmLock(state, 'slack:DPRIVATE:', async () => {
      throw new Error('failure')
    })).rejects.toThrow('failure')
    await expect(withUnifiedDmLock(state, 'slack:DPRIVATE:', async () => 'ok')).resolves.toBe('ok')
  })
})
