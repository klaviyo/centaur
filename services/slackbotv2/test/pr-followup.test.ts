import { describe, expect, test } from 'bun:test'
import {
  deliverPrFollowupUpdate,
  PR_FOLLOWUP_CURSOR_KEY,
  prFollowupFromMetadata,
  prFollowupOutcome,
  prFollowupUpdateText,
  sweepPrFollowupExecutions,
  type PrFollowupExecution,
  type PrFollowupMetadata,
  type PrFollowupThread,
  type PrFollowupTurnResult
} from '../src/pr-followup'
import type { SlackbotV2Options, SlackbotV2ThreadState } from '../src/types'

const THREAD = 'slack:C123:1700000000.000100'

function meta(kind: string, directRequest = false): PrFollowupMetadata {
  return { repository: 'acme/widgets', number: 42, followupId: `fu-${kind}`, kind, directRequest }
}

function execution(kind: string, input: Partial<PrFollowupExecution> = {}): PrFollowupExecution {
  return {
    executionId: `exec-${kind}`,
    threadKey: THREAD,
    status: 'completed',
    prFollowup: meta(kind, kind === 'review' || kind.endsWith('comment')),
    ...input
  }
}

function makeThread(initial: SlackbotV2ThreadState | null, id = THREAD) {
  let state = initial
  const posts: string[] = []
  const thread: PrFollowupThread = {
    id,
    get state() {
      return Promise.resolve(state)
    },
    async setState(update) {
      state = { ...(state ?? {}), ...update }
    },
    async post(message) {
      posts.push(message.markdown)
    }
  }
  return { thread, posts, current: () => state }
}

// A thread slackbot already ran a turn in for Slack user U1.
const ownedState: SlackbotV2ThreadState = {
  executedMessageIds: ['1700000000.000100'],
  historyForwarded: true
}

const options = { apiUrl: 'http://api.test', botToken: 'xoxb-test', signingSecret: 'test' } as SlackbotV2Options

const result = (text: string, failed = false): PrFollowupTurnResult => ({ text, failed })

describe('pr_followup metadata', () => {
  test('parses server-set metadata and rejects malformed shapes', () => {
    expect(
      prFollowupFromMetadata({
        pr_followup: {
          repository: 'acme/widgets',
          number: 42,
          followup_id: 'fu-1',
          kind: 'review',
          reply_target: { type: 'github_comment', repository: 'acme/widgets', number: 42, review_id: 9 }
        }
      })
    ).toEqual({ repository: 'acme/widgets', number: 42, followupId: 'fu-1', kind: 'review', directRequest: true })
    expect(prFollowupFromMetadata({})).toBeUndefined()
    expect(prFollowupFromMetadata({ pr_followup: { repository: 'nope', number: 1, followup_id: 'x', kind: 'review' } }))
      .toBeUndefined()
    expect(prFollowupFromMetadata({ pr_followup: { repository: 'a/b', number: 0, followup_id: 'x', kind: 'review' } }))
      .toBeUndefined()
  })
})

describe('which outcomes are worth a Slack update', () => {
  test('routine CI that needs nothing stays quiet', () => {
    expect(prFollowupOutcome(meta('ci_failure'), result('Flaky; reran.\nPR-FOLLOWUP-STATUS: done'))).toBeUndefined()
    expect(prFollowupOutcome(meta('ci_failure'), result('No status line at all'))).toBeUndefined()
    expect(prFollowupOutcome(meta('conflict'), result('PR-FOLLOWUP-STATUS: done'))).toBeUndefined()
  })

  test('prepare-and-ask is a blocker asking for an authorized human push, with the PR link', () => {
    const m = meta('ci_failure')
    const r = result('Fixed the import.\n`PR-FOLLOWUP-STATUS: needs-push`')
    expect(prFollowupOutcome(m, r)).toBe('blocker')
    const text = prFollowupUpdateText(m, 'blocker', r)
    expect(text).toContain('[acme/widgets#42](https://github.com/acme/widgets/pull/42)')
    expect(text).toContain('push it')
  })

  test('a failed run or an explicit block is a blocker', () => {
    expect(prFollowupOutcome(meta('review', true), result('', true))).toBe('blocker')
    expect(prFollowupOutcome(meta('review', true), result('PR-FOLLOWUP-STATUS: blocked'))).toBe('blocker')
  })

  test('a finished direct GitHub request is a completion; a push is a push', () => {
    expect(prFollowupOutcome(meta('review', true), result('Replied.\nPR-FOLLOWUP-STATUS: done'))).toBe('completion')
    expect(prFollowupOutcome(meta('ci_failure'), result('PR-FOLLOWUP-STATUS: pushed'))).toBe('push')
  })
})

describe('deliverPrFollowupUpdate', () => {
  test('posts one brief update, never the transcript, and never twice', async () => {
    const { thread, posts } = makeThread({ ...ownedState })
    let collected = 0
    const collect = async () => {
      collected++
      return result('Long transcript the thread must not see.\nPR-FOLLOWUP-STATUS: needs-push')
    }
    expect(await deliverPrFollowupUpdate(options, thread, execution('ci_failure'), collect)).toBe('posted')
    expect(await deliverPrFollowupUpdate(options, thread, execution('ci_failure'), collect)).toBe('duplicate')
    expect(posts).toHaveLength(1)
    expect(posts[0]).not.toContain('Long transcript')
    expect(collected).toBe(1)
  })

  test('routine CI records the execution and posts nothing', async () => {
    const { thread, posts, current } = makeThread({ ...ownedState })
    const outcome = await deliverPrFollowupUpdate(options, thread, execution('ci_failure'), async () =>
      result('PR-FOLLOWUP-STATUS: done'))
    expect(outcome).toBe('quiet')
    expect(posts).toEqual([])
    expect(current()?.prFollowupUpdates).toEqual(['exec-ci_failure'])
  })

  test('a cancelled follow-up is quiet without reading its output', async () => {
    const { thread, posts } = makeThread({ ...ownedState })
    const outcome = await deliverPrFollowupUpdate(options, thread, execution('review', { status: 'cancelled' }), () => {
      throw new Error('should not collect')
    })
    expect(outcome).toBe('quiet')
    expect(posts).toEqual([])
  })

  test('never posts into a thread slackbot does not already own', async () => {
    const fresh = makeThread(null)
    expect(
      await deliverPrFollowupUpdate(options, fresh.thread, execution('review'), async () => result('done'))
    ).toBe('not_owned')
    expect(fresh.posts).toEqual([])

    // A different thread than the session's own key, even one slackbot owns.
    const other = makeThread({ ...ownedState }, 'slack:C999:1700000000.000999')
    expect(
      await deliverPrFollowupUpdate(options, other.thread, execution('review'), async () => result('done'))
    ).toBe('not_owned')
    expect(other.posts).toEqual([])
  })
})

describe('sweepPrFollowupExecutions', () => {
  function listingFetch(calls: string[], status = 200) {
    return async (input: RequestInfo | URL) => {
      calls.push(String(input))
      if (status !== 200) return new Response('not found', { status })
      return new Response(
        JSON.stringify({
          executions: [
            {
              execution_id: 'exec-1',
              thread_key: THREAD,
              status: 'completed',
              metadata: {
                pr_followup: {
                  repository: 'acme/widgets',
                  number: 42,
                  followup_id: 'fu-1',
                  kind: 'review',
                  reply_target: { type: 'github_comment', repository: 'acme/widgets', number: 42, review_id: 9 }
                },
                pr_followup_github_user_id: '501',
                push_authorization: 'prepare_and_ask'
              }
            },
            { execution_id: 'exec-2', thread_key: THREAD, status: 'completed', metadata: {} }
          ],
          next_cursor: 'cursor-2'
        }),
        { status: 200, headers: { 'content-type': 'application/json' } }
      )
    }
  }

  function memoryState() {
    const values = new Map<string, unknown>()
    return {
      values,
      async get<T>(key: string) {
        return values.get(key) as T | undefined
      },
      async set(key: string, value: unknown) {
        values.set(key, value)
      }
    }
  }

  test('delivers listed follow-ups once, advances the cursor, and makes no session writes', async () => {
    const calls: string[] = []
    const state = memoryState()
    const { thread, posts, current } = makeThread({ ...ownedState })
    const deps = {
      options: { ...options, fetch: listingFetch(calls) } as SlackbotV2Options,
      state,
      thread: () => thread,
      collectResult: async () => result('Replied on GitHub.\nPR-FOLLOWUP-STATUS: done')
    }
    expect(await sweepPrFollowupExecutions(deps)).toBe(1)
    expect(await sweepPrFollowupExecutions(deps)).toBe(1)

    expect(posts).toEqual([
      'Answered a review on [acme/widgets#42](https://github.com/acme/widgets/pull/42) in GitHub.'
    ])
    expect(state.values.get(PR_FOLLOWUP_CURSOR_KEY)).toBe('cursor-2')
    expect(calls[1]).toContain('after=cursor-2')
    // Discovery never appends, executes, or attaches a Slack requester: the
    // follow-up keeps the GitHub identity api-rs dispatched it with, and the
    // thread's earlier Slack participant is not involved.
    expect(calls.every(url => new URL(url).pathname === '/api/pr-followups/executions')).toBe(true)
    expect(current()?.executedMessageIds).toEqual(ownedState.executedMessageIds)
  })

  test('idles when api-rs does not serve the listing', async () => {
    const calls: string[] = []
    const result = await sweepPrFollowupExecutions({
      options: { ...options, fetch: listingFetch(calls, 404) } as SlackbotV2Options,
      state: memoryState(),
      thread: () => makeThread(null).thread,
      collectResult: async () => ({ text: '', failed: false })
    })
    expect(result).toBeNull()
  })
})
