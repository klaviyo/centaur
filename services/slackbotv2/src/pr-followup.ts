import type { SlackbotV2Options, SlackbotV2ThreadState } from './types'
import { errorMessage, isJsonObject, stringValue, traceLog, traceWarn } from './utils'
import { withSlackApiTimeout } from './session-api'

/**
 * Brief Slack updates for PR follow-up executions (C10). api-rs dispatches
 * GitHub follow-ups (reviews, mentions, red CI) into the Slack session that
 * opened the PR, tagging each execution with server-set `pr_followup`
 * metadata. Slack never renders those transcripts into the thread: githubbot
 * answers direct requests on GitHub, and this module posts at most one short
 * message per execution, only for a meaningful push, blocker, or completion.
 * Routine CI stays quiet. User steering and identities are untouched: this
 * path never appends, executes, or resolves a requester.
 */

export type PrFollowupMetadata = {
  repository: string
  number: number
  followupId: string
  kind: string
  /** A direct GitHub request (comment or review) that githubbot answers on GitHub. */
  directRequest: boolean
}

export type PrFollowupOutcome = 'push' | 'blocker' | 'completion'

/** A terminal PR follow-up execution on a Slack session, as api-rs lists it. */
export type PrFollowupExecution = {
  executionId: string
  threadKey: string
  status: string
  prFollowup: PrFollowupMetadata
}

export type PrFollowupTurnResult = { text: string; failed: boolean }

export const PR_FOLLOWUP_STATUS_PREFIX = 'PR-FOLLOWUP-STATUS:'
const ROUTINE_KINDS = new Set(['ci_failure', 'conflict'])
const DELIVERED_IDS_MAX = 200

/** Parse the server-set `pr_followup` execute metadata, if present and well formed. */
export function prFollowupFromMetadata(metadata: unknown): PrFollowupMetadata | undefined {
  const raw = isJsonObject(metadata) && isJsonObject(metadata.pr_followup) ? metadata.pr_followup : undefined
  if (!raw) return undefined
  const repository = stringValue(raw.repository)
  const number = typeof raw.number === 'number' && Number.isSafeInteger(raw.number) ? raw.number : undefined
  const followupId = stringValue(raw.followup_id)
  const kind = stringValue(raw.kind)
  if (!repository || !/^[^/\s]+\/[^/\s]+$/.test(repository) || !number || number <= 0 || !followupId || !kind) {
    return undefined
  }
  return { repository, number, followupId, kind, directRequest: isJsonObject(raw.reply_target) }
}

/** The status the prepare-and-ask preamble asks the agent to end with. */
export function prFollowupStatus(text: string): 'needs-push' | 'blocked' | 'done' | 'pushed' | undefined {
  let status: ReturnType<typeof prFollowupStatus>
  for (const line of text.split('\n')) {
    const clean = line.replace(/[`*_]/g, '').trim()
    if (!clean.startsWith(PR_FOLLOWUP_STATUS_PREFIX)) continue
    const value = clean.slice(PR_FOLLOWUP_STATUS_PREFIX.length).trim().toLowerCase()
    if (value === 'needs-push' || value === 'blocked' || value === 'done' || value === 'pushed') status = value
  }
  return status
}

/**
 * Whether an execution deserves a Slack update. In v1 a prepared change waits
 * on a human push (prepare-and-ask), which is a blocker; a failed run is a
 * blocker; a direct GitHub request that finished is a completion; routine CI
 * or conflict work that needs nothing from anyone produces nothing.
 */
export function prFollowupOutcome(
  meta: PrFollowupMetadata,
  result: PrFollowupTurnResult
): PrFollowupOutcome | undefined {
  if (result.failed) return 'blocker'
  const status = prFollowupStatus(result.text)
  if (status === 'needs-push' || status === 'blocked') return 'blocker'
  if (status === 'pushed') return 'push'
  if (ROUTINE_KINDS.has(meta.kind)) return undefined
  return meta.directRequest ? 'completion' : undefined
}

export function prFollowupUpdateText(
  meta: PrFollowupMetadata,
  outcome: PrFollowupOutcome,
  result: PrFollowupTurnResult
): string {
  const link = `[${meta.repository}#${meta.number}](https://github.com/${meta.repository}/pull/${meta.number})`
  const what = followupLabel(meta.kind)
  if (outcome === 'push') return `Pushed an update to ${link} for ${what}.`
  if (outcome === 'completion') return `Answered ${what} on ${link} in GitHub.`
  if (result.failed) return `I couldn't finish ${what} on ${link}. Details are in the session; a human may need to look.`
  if (prFollowupStatus(result.text) === 'needs-push') {
    return `I prepared a fix for ${what} on ${link}, but I can't push it myself. Could someone with access to the branch push it?`
  }
  return `I'm blocked on ${what} on ${link} and need a human to take a look.`
}

function followupLabel(kind: string): string {
  switch (kind) {
    case 'review':
      return 'a review'
    case 'review_comment':
    case 'issue_comment':
    case 'mention':
      return 'a GitHub comment'
    case 'ci_failure':
      return 'failing CI'
    case 'conflict':
      return 'merge conflicts'
    default:
      return 'a GitHub follow-up'
  }
}

export type PrFollowupThread = {
  id: string
  state: Promise<SlackbotV2ThreadState | null>
  setState(state: Partial<SlackbotV2ThreadState>): Promise<void>
  post(message: { markdown: string }): Promise<unknown>
}

export type PrFollowupDeliveryOutcome = 'posted' | 'quiet' | 'duplicate' | 'not_owned'

/**
 * Post at most one brief update for a terminal PR follow-up execution.
 * Only in the session's own Slack thread, and only when slackbot already owns
 * that thread (it ran a turn there), so GitHub activity never reaches a channel
 * the original session didn't already use. Never renders the transcript.
 */
export async function deliverPrFollowupUpdate(
  options: SlackbotV2Options,
  thread: PrFollowupThread,
  execution: PrFollowupExecution,
  collectResult: () => Promise<PrFollowupTurnResult>
): Promise<PrFollowupDeliveryOutcome> {
  const fields = {
    execution_id: execution.executionId,
    followup_kind: execution.prFollowup.kind,
    thread_id: thread.id
  }
  if (thread.id !== execution.threadKey || !execution.threadKey.startsWith('slack:')) {
    traceLog(options, 'slackbotv2_pr_followup_not_owned', undefined, fields)
    return 'not_owned'
  }
  const state = (await thread.state) ?? {}
  const owned = state.historyForwarded === true || (state.executedMessageIds?.length ?? 0) > 0
  if (!owned) {
    traceLog(options, 'slackbotv2_pr_followup_not_owned', undefined, fields)
    return 'not_owned'
  }
  const delivered = state.prFollowupUpdates ?? []
  if (delivered.includes(execution.executionId)) return 'duplicate'

  // A cancelled run (a stop, a superseding dispatch) is not news.
  const result: PrFollowupTurnResult | undefined =
    execution.status === 'cancelled' ? undefined : await collectResult()
  const outcome = result ? prFollowupOutcome(execution.prFollowup, result) : undefined
  // Record before posting: a crash between the two drops one brief update
  // rather than ever posting it twice.
  await thread.setState({
    prFollowupUpdates: [...delivered, execution.executionId].slice(-DELIVERED_IDS_MAX)
  })
  if (!outcome || !result) {
    traceLog(options, 'slackbotv2_pr_followup_quiet', undefined, fields)
    return 'quiet'
  }
  const text = prFollowupUpdateText(execution.prFollowup, outcome, result)
  await withSlackApiTimeout(options, 'post pr follow-up update', () => thread.post({ markdown: text }))
  traceLog(options, 'slackbotv2_pr_followup_posted', undefined, { ...fields, outcome })
  return 'posted'
}

// ---------------------------------------------------------------------------
// Discovery. api-rs starts these executions itself, so slackbot polls for
// terminal PR follow-up executions on `slack:` sessions after a cursor kept in
// its state store.
// ---------------------------------------------------------------------------

export const PR_FOLLOWUP_CURSOR_KEY = 'slackbotv2:pr-followup-cursor'

export type PrFollowupListing = { executions: PrFollowupExecution[]; nextCursor?: string }

/**
 * List terminal PR follow-up executions on Slack sessions after `cursor`.
 * Returns null when api-rs doesn't serve the listing (404), so the sweep
 * idles instead of failing.
 */
export async function listPrFollowupExecutions(
  options: SlackbotV2Options,
  cursor: string | undefined
): Promise<PrFollowupListing | null> {
  const fetchFn = options.fetch ?? fetch
  const url = new URL('/api/pr-followups/executions', options.apiUrl.endsWith('/') ? options.apiUrl : `${options.apiUrl}/`)
  url.searchParams.set('thread_prefix', 'slack:')
  if (cursor) url.searchParams.set('after', cursor)
  const apiKey = options.apiKey ?? process.env.SLACKBOT_API_KEY
  const response = await fetchFn(url.toString(), {
    method: 'GET',
    headers: apiKey ? { authorization: `Bearer ${apiKey}` } : {}
  })
  if (response.status === 404) return null
  if (!response.ok) throw new Error(`list pr follow-up executions failed: ${response.status}`)
  const payload: unknown = await response.json()
  if (!isJsonObject(payload) || !Array.isArray(payload.executions)) {
    throw new Error('list pr follow-up executions returned an unexpected body')
  }
  const executions: PrFollowupExecution[] = []
  for (const item of payload.executions) {
    if (!isJsonObject(item)) continue
    const executionId = stringValue(item.execution_id)
    const threadKey = stringValue(item.thread_key)
    const prFollowup = prFollowupFromMetadata(item.metadata)
    if (!executionId || !threadKey || !prFollowup) continue
    executions.push({ executionId, threadKey, status: stringValue(item.status) ?? 'completed', prFollowup })
  }
  return { executions, nextCursor: stringValue(payload.next_cursor) }
}

export type PrFollowupSweepDeps = {
  options: SlackbotV2Options
  state: {
    get<T>(key: string): Promise<T | null | undefined>
    set(key: string, value: unknown): Promise<unknown>
  }
  thread(threadKey: string): PrFollowupThread
  collectResult(execution: PrFollowupExecution): Promise<PrFollowupTurnResult>
}

/** One discovery pass. Returns how many executions were examined, or null when unsupported. */
export async function sweepPrFollowupExecutions(deps: PrFollowupSweepDeps): Promise<number | null> {
  const cursor = (await deps.state.get<string>(PR_FOLLOWUP_CURSOR_KEY)) ?? undefined
  const listing = await listPrFollowupExecutions(deps.options, cursor)
  if (!listing) return null
  for (const execution of listing.executions) {
    try {
      await deliverPrFollowupUpdate(
        deps.options,
        deps.thread(execution.threadKey),
        execution,
        () => deps.collectResult(execution)
      )
    } catch (error) {
      // One bad thread must not block the rest; its id stays undelivered only
      // if the failure happened before the dedupe record.
      traceWarn(deps.options, 'slackbotv2_pr_followup_delivery_failed', undefined, {
        error: errorMessage(error),
        execution_id: execution.executionId
      })
    }
  }
  if (listing.nextCursor && listing.nextCursor !== cursor) {
    await deps.state.set(PR_FOLLOWUP_CURSOR_KEY, listing.nextCursor)
  }
  return listing.executions.length
}
