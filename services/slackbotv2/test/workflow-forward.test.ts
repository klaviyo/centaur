import { describe, expect, it } from 'bun:test'
import type { SlackbotV2Options } from '../src/types'
import { forwardSlackEventToWorkflow, parseWorkflowWebhookSlug } from '../src/workflow-forward'

type Captured = { url: string; init: RequestInit }

function optionsWith(
  respond: () => Response | Promise<Response>,
  calls: Captured[],
  logs: string[] = []
): SlackbotV2Options {
  const log = (message: string) => { logs.push(message) }
  return {
    apiKey: 'slackbot-api-key',
    apiUrl: 'http://api.test:8080',
    botToken: 'xoxb-test',
    signingSecret: 'signing-secret',
    workflowWebhookSlug: 'slack-channel',
    fetch: async (input, init) => {
      calls.push({ url: String(input), init: init ?? {} })
      return respond()
    },
    logger: { debug: log, info: log, warn: log, error: log, child() { return this } }
  } as SlackbotV2Options
}

function envelope(event: Record<string, unknown>, type = 'event_callback'): string {
  return JSON.stringify({ type, event_id: 'Ev1', team_id: 'T1', event })
}

describe('forwardSlackEventToWorkflow', () => {
  it('posts the verified raw body with the slackbot api-rs credentials', async () => {
    const calls: Captured[] = []
    const rawBody = envelope({ type: 'message', channel: 'C1', ts: '1.000100', text: 'hi' })
    await forwardSlackEventToWorkflow(rawBody, optionsWith(() => Response.json({ ok: true }), calls))

    expect(calls).toHaveLength(1)
    expect(calls[0]!.url).toBe('http://api.test:8080/api/webhooks/slack-channel')
    expect(calls[0]!.init.method).toBe('POST')
    expect(calls[0]!.init.body).toBe(rawBody)
    expect(calls[0]!.init.headers).toEqual({
      authorization: 'Bearer slackbot-api-key',
      'content-type': 'application/json'
    })
  })

  it('skips non-message events, other envelope types and unparseable bodies', () => {
    const calls: Captured[] = []
    const options = optionsWith(() => Response.json({ ok: true }), calls)
    for (const rawBody of [
      envelope({ type: 'app_mention', channel: 'C1', ts: '1.000100' }),
      envelope({ type: 'reaction_added' }),
      JSON.stringify({ type: 'url_verification', challenge: 'x' }),
      'payload=%7B%22type%22%3A%22block_actions%22%7D',
      'not json'
    ]) {
      expect(forwardSlackEventToWorkflow(rawBody, options)).toBeNull()
    }
    expect(calls).toHaveLength(0)
  })

  it('does nothing when no workflow webhook slug is configured', () => {
    const calls: Captured[] = []
    const options = { ...optionsWith(() => Response.json({}), calls), workflowWebhookSlug: undefined }
    expect(forwardSlackEventToWorkflow(envelope({ type: 'message' }), options)).toBeNull()
    expect(calls).toHaveLength(0)
  })

  it('logs rejected and failed forwards without throwing', async () => {
    const rawBody = envelope({ type: 'message', channel: 'C1', ts: '1.000100' })
    for (const respond of [
      () => new Response('unauthorized', { status: 401 }),
      () => Promise.reject(new Error('connection refused'))
    ]) {
      const logs: string[] = []
      await forwardSlackEventToWorkflow(rawBody, optionsWith(respond, [], logs))
      expect(logs).toContain('slackbotv2_workflow_forward_failed')
    }
  })
})

describe('parseWorkflowWebhookSlug', () => {
  it('leaves forwarding off when unset', () => {
    const warnings: string[] = []
    expect(parseWorkflowWebhookSlug(undefined, reason => { warnings.push(reason) })).toBeUndefined()
    expect(warnings).toHaveLength(0)
    expect(parseWorkflowWebhookSlug('slack-channel', reason => { warnings.push(reason) })).toBe('slack-channel')
  })

  it('rejects slugs that could change the target path', () => {
    const warnings: string[] = []
    for (const slug of ['../sessions', 'a/b', 'Slack', 'x?y=1']) {
      expect(parseWorkflowWebhookSlug(slug, reason => { warnings.push(reason) })).toBeUndefined()
    }
    expect(warnings).toHaveLength(4)
  })
})
