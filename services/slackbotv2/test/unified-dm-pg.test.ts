import { randomUUID } from 'node:crypto'
import { afterAll, beforeAll, describe, expect, it } from 'bun:test'
import { createPostgresState } from '@chat-adapter/state-pg'
import pg from 'pg'
import { withUnifiedDmLock } from '../src/unified-dm'

// Use a disposable database. This exercises the production SDK's existing
// tables with independent connections; it adds no Centaur schema migration.
const databaseUrl = process.env.SLACKBOTV2_TEST_DATABASE_URL

describe.skipIf(!databaseUrl)('unified DM PostgreSQL coordination', () => {
  const keyPrefix = `unified-dm-test-${randomUUID()}`
  const url = databaseUrl ?? 'postgresql://localhost/unified_dm_validation'
  const first = createPostgresState({ url, keyPrefix })
  const second = createPostgresState({ url, keyPrefix })

  beforeAll(async () => {
    await first.connect()
    await second.connect()
  })

  afterAll(async () => {
    await Promise.all([first.disconnect(), second.disconnect()])
    const pool = new pg.Pool({ connectionString: databaseUrl })
    try {
      await pool.query('DELETE FROM chat_state_locks WHERE key_prefix = $1', [keyPrefix])
      await pool.query('DELETE FROM chat_state_cache WHERE key_prefix = $1', [keyPrefix])
    } finally {
      await pool.end()
    }
  })

  it('serializes read-modify-write handoffs across independent adapters', async () => {
    const threadId = 'slack:DCOORDINATION:'
    await first.set('counter', 0)
    await Promise.all(Array.from({ length: 12 }, (_, index) =>
      withUnifiedDmLock(index % 2 === 0 ? first : second, threadId, async () => {
        const adapter = index % 2 === 0 ? first : second
        const previous = await adapter.get<number>('counter')
        await adapter.set('counter', (previous ?? 0) + 1)
      }, { waitMs: 5000, pollMs: 10 })
    ))
    expect(await second.get('counter')).toBe(12)
  })

  it('renews the lease and rejects a contender without running its work', async () => {
    const threadId = 'slack:DRENEWAL:'
    const started = Promise.withResolvers<void>()
    const release = Promise.withResolvers<void>()
    const owner = withUnifiedDmLock(first, threadId, async () => {
      started.resolve()
      await release.promise
    }, { leaseMs: 600, refreshMs: 100 })
    let contenderRan = false
    try {
      await started.promise
      await new Promise(resolve => setTimeout(resolve, 1000))
      await expect(withUnifiedDmLock(second, threadId, async () => {
        contenderRan = true
      }, { waitMs: 50, pollMs: 10 })).rejects.toThrow()
      expect(contenderRan).toBe(false)
      // Another DM stays independent while this one is busy.
      expect(await withUnifiedDmLock(second, 'slack:DINDEPENDENT:', async () => 'ok')).toBe('ok')
    } finally {
      release.resolve()
      await owner
    }
    expect(await withUnifiedDmLock(second, threadId, async () => 'released')).toBe('released')
  })

  it('cannot release a successor with an expired owner token', async () => {
    const lockKey = 'slackbotv2:unified-dm:handoff:slack:DSTALEOWNER:'
    const staleOwner = await first.acquireLock(lockKey, 50)
    expect(staleOwner).not.toBeNull()
    await new Promise(resolve => setTimeout(resolve, 100))
    const successor = await second.acquireLock(lockKey, 5000)
    expect(successor).not.toBeNull()
    try {
      await first.releaseLock(staleOwner!)
      expect(await first.acquireLock(lockKey, 5000)).toBeNull()
    } finally {
      await second.releaseLock(successor!)
    }
  })
})
