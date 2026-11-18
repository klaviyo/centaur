import { apiHeaders, fetchWithTimeout } from './session-api'
import type { SlackbotV2Options } from './types'
import { elapsedMs, errorMessage, isJsonObject, nowMs, stringValue, traceLog, traceWarn } from './utils'
import { parseSlackWebhookPayload } from './slack-events'

const WORKFLOW_FORWARD_TIMEOUT_MS = 3_000
const WEBHOOK_SLUG_PATTERN = /^[a-z0-9][a-z0-9_-]{0,63}$/

/**
 * Forward verified Slack message events to one api-rs workflow webhook.
 *
 * Slack can deliver Events API traffic to only one URL, so slackbotv2 relays
 * the already-verified raw body with its existing api-rs ingress credentials;
 * the workflow webhook declares bearer auth against SLACKBOT_API_KEY. api-rs
 * applies the workflow's filter and body-hash dedupe; Slack retries resend
 * identical bytes, so a redelivered event does not create a second run.
 * Best-effort: failures are logged and never change the Slack webhook response.
 */
export function forwardSlackEventToWorkflow(
  rawBody: string,
  options: SlackbotV2Options
): Promise<void> | null {
  const slug = options.workflowWebhookSlug
  if (!slug) return null
  const payload = parseSlackWebhookPayload(rawBody)
  if (!payload || payload.type !== 'event_callback' || !isJsonObject(payload.event)) return null
  if (payload.event.type !== 'message') return null

  return (async () => {
    const startedAtMs = nowMs()
    const fields = {
      slack_event_id: stringValue(payload.event_id),
      workflow_webhook_slug: slug
    }
    try {
      const response = await fetchWithTimeout(
        options.fetch ?? globalThis.fetch,
        new URL(`/api/webhooks/${slug}`, options.apiUrl),
        { body: rawBody, headers: apiHeaders(options), method: 'POST' },
        WORKFLOW_FORWARD_TIMEOUT_MS,
        'forward Slack event to workflow webhook'
      )
      await response.body?.cancel()
      if (!response.ok) throw new Error(`workflow webhook returned status ${response.status}`)
      traceLog(options, 'slackbotv2_workflow_forward_complete', undefined, {
        ...fields,
        phase_ms: elapsedMs(startedAtMs),
        status: response.status
      })
    } catch (error) {
      traceWarn(options, 'slackbotv2_workflow_forward_failed', undefined, {
        ...fields,
        error: errorMessage(error),
        phase_ms: elapsedMs(startedAtMs)
      })
    }
  })()
}

/** Accept only a plain webhook slug so configuration cannot redirect the path. */
export function parseWorkflowWebhookSlug(
  slug: string | undefined,
  warn: (reason: string) => void
): string | undefined {
  if (!slug) return undefined
  if (WEBHOOK_SLUG_PATTERN.test(slug)) return slug
  warn('SLACKBOTV2_WORKFLOW_WEBHOOK_SLUG is not a valid webhook slug; forwarding disabled')
  return undefined
}
