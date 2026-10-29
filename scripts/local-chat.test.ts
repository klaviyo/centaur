import { afterAll, beforeAll, describe, expect, test } from 'bun:test'
import { createHmac } from 'node:crypto'
import { BOT, TEAM, USER, USER_B } from '../e2e/fakes/slack-fixture'
import { LocalChat, Transcript, localUrl, parseInput, safeText, type Snapshot } from './local-chat'

describe('local chat input and display', () => {
  test('keeps fixture requests on loopback', () => {
    expect(localUrl('http://127.0.0.1:30443')).toBe('http://127.0.0.1:30443')
    expect(localUrl('http://[::1]:30443/')).toBe('http://[::1]:30443')
    for (const url of ['https://slack.com', 'http://localhost.evil.example', 'http://localhost/api', 'http://user:password@localhost', 'file:///tmp/slack', 'http://localhost?token=value']) {
      expect(() => localUrl(url)).toThrow()
    }
  })

  test('stop travels as a normal bare message and leading slash can be escaped', () => {
    expect(parseInput('/stop')).toEqual({ kind: 'send', text: 'stop' })
    expect(parseInput('//something')).toEqual({ kind: 'send', text: '/something' })
    expect(parseInput('/status')).toEqual({ kind: 'status' })
    expect(parseInput('  continue working  ')).toEqual({ kind: 'send', text: 'continue working' })
    expect(parseInput('/surprise')).toEqual({ kind: 'unknown' })
  })

  test('shows changed replies once, hides placeholders, and strips terminal controls', () => {
    const transcript = new Transcript()
    const snapshot: Snapshot = {
      messages: [{ ts: '1.0', user: BOT.userId, text: '...', streaming: true }],
      ephemerals: [], status: ''
    }
    expect(transcript.changes(snapshot)).toEqual([])
    snapshot.messages[0] = { ts: '1.0', user: BOT.userId, text: '\u001b[2JHello', streaming: false }
    expect(transcript.changes(snapshot)).toEqual(['Centaur (updated):\nHello'])
    expect(transcript.changes(snapshot)).toEqual([])
    snapshot.ephemerals.push({ channel: 'DE2EUSER1', text: 'Stopped.' })
    expect(transcript.changes(snapshot)).toEqual(['Notice: Stopped.'])
    expect(safeText('Hello\u0007')).toBe('Hello')
    expect(safeText('line 1\nline 2\r\u001b[31mred\u001b[0m')).toBe('line 1\nline 2red')
  })
})

describe('actual fake Slack transport', () => {
  // These are deliberately synthetic fixture values, never live credentials.
  const signingSecret = 'local-cli-test-signing-fixture'
  const botToken = 'local-cli-test-bot-fixture'
  let webhook: ReturnType<typeof Bun.serve>
  let fakeSlack: ReturnType<typeof Bun.spawn>
  let chat: LocalChat
  const events: Array<{ body: string; signature: string; timestamp: string }> = []

  beforeAll(async () => {
    webhook = Bun.serve({
      port: 0,
      hostname: '127.0.0.1',
      async fetch(request) {
        events.push({ body: await request.text(), signature: request.headers.get('x-slack-signature') ?? '', timestamp: request.headers.get('x-slack-request-timestamp') ?? '' })
        return new Response('ok')
      }
    })
    const reservation = Bun.serve({ port: 0, hostname: '127.0.0.1', fetch: () => new Response('unused') })
    const port = reservation.port!
    await reservation.stop(true)
    fakeSlack = Bun.spawn([process.execPath, new URL('../e2e/fakes/fake-slack.ts', import.meta.url).pathname], {
      env: { ...process.env, PORT: String(port), SLACK_BOT_TOKEN: botToken, SLACK_SIGNING_SECRET: signingSecret, SLACK_EVENTS_URL: `http://127.0.0.1:${webhook.port}/webhook` },
      stdout: 'ignore', stderr: 'ignore'
    })
    chat = new LocalChat(`http://127.0.0.1:${port}`)
    await eventually(async () => {
      try { await chat.connect(); return true } catch { return false }
    })
  })

  afterAll(async () => {
    fakeSlack?.kill()
    if (fakeSlack) await fakeSlack.exited
    await webhook?.stop(true)
  })

  test('two top-level messages and stop use the stable DM and signed ingress envelope', async () => {
    expect(chat.channel).toBe('DE2EUSER1')
    expect(chat.sessionKey).toBe('slack:DE2EUSER1:')
    for (const text of ['First message', 'Second message', 'stop']) await chat.send(text)
    await eventually(() => events.length === 3)
    expect(events.map(({ body }) => JSON.parse(body).event.text)).toEqual(['First message', 'Second message', 'stop'])
    for (const event of events) {
      const expected = createHmac('sha256', signingSecret).update(`v0:${event.timestamp}:${event.body}`).digest('hex')
      expect(event.signature).toBe(`v0=${expected}`)
      const payload = JSON.parse(event.body)
      expect(payload.team_id).toBe(TEAM.id)
      expect(payload.event.user).toBe(USER.id)
      expect(payload.event.channel_type).toBe('im')
      expect(payload.event.channel).toBe(chat.channel)
      expect(payload.event.thread_ts).toBeUndefined()
    }
  })

  test('reads bot edits, ephemerals, and threaded fallback replies', async () => {
    const posted = await botApi('chat.postMessage', { channel: chat.channel, text: '...' })
    const transcript = new Transcript()
    transcript.remember(await chat.snapshot())
    await botApi('chat.update', { channel: chat.channel, ts: posted.ts, text: 'Continuous chat reply.' })
    await botApi('chat.postEphemeral', { channel: chat.channel, user: USER.id, text: 'A private notice.' })
    const snapshot = await chat.snapshot()
    expect(transcript.changes(snapshot)).toEqual(['Centaur (updated):\nContinuous chat reply.', 'Notice: A private notice.'])
    const root = snapshot.messages[0]!
    await botApi('chat.postMessage', { channel: chat.channel, thread_ts: root.ts, text: 'Threaded fallback reply.' })
    expect((await chat.snapshot()).messages.some(message => message.text === 'Threaded fallback reply.')).toBe(true)
  })

  test('bot posts addressed to fixture users appear only in their stable DMs', async () => {
    const transcript = new Transcript()
    transcript.remember(await chat.snapshot())
    const posted = await botApi<{ ok: boolean; channel: string; message: { channel: string; user: string } }>('chat.postMessage', {
      channel: USER.id, text: 'Your task has finished.'
    })
    expect(posted.ok).toBe(true)
    expect(posted.channel).toBe(chat.channel)
    expect(posted.message.channel).toBe(chat.channel)
    expect(posted.message.user).toBe(BOT.userId)
    const other = await botApi<{ channel: string }>('chat.postMessage', { channel: USER_B.id, text: 'Another user’s task.' })
    expect(other.channel).toBe(`D${USER_B.id.slice(1)}`)
    expect(transcript.changes(await chat.snapshot())).toEqual(['Centaur:\nYour task has finished.'])
    expect(await botApi('chat.postMessage', { channel: 'UUNKNOWN', text: 'Must not be delivered.' })).toEqual({ ok: false, error: 'channel_not_found' })
    expect(await botApi('conversations.info', { channel: USER.id })).toEqual({ ok: false, error: 'channel_not_found' })
  })

  test('the CLI accepts commands and exits cleanly with piped input', async () => {
    const cli = Bun.spawn([process.execPath, new URL('./local-chat.ts', import.meta.url).pathname, '--url', chat.url], {
      stdin: 'pipe', stdout: 'pipe', stderr: 'pipe'
    })
    try {
      cli.stdin.write('/status\n/quit\n')
      cli.stdin.end()
      const output = await new Response(cli.stdout).text()
      expect(await new Response(cli.stderr).text()).toBe('')
      expect(await cli.exited).toBe(0)
      expect(output).toContain(`Connected to ${chat.url}`)
      expect(output).toContain('Expected unified session: slack:DE2EUSER1:')
      expect(output).toContain('Disconnected.')
    } finally {
      cli.kill()
    }
  })

  async function botApi<T = { ts: string }>(method: string, body: Record<string, unknown>): Promise<T> {
    const response = await fetch(`${chat.url}/api/${method}`, {
      method: 'POST', headers: { authorization: `Bearer ${botToken}`, 'content-type': 'application/json' }, body: JSON.stringify(body)
    })
    return response.json() as Promise<T>
  }
})

async function eventually(check: () => boolean | Promise<boolean>): Promise<void> {
  const until = Date.now() + 5_000
  while (Date.now() < until) {
    if (await check()) return
    await Bun.sleep(25)
  }
  throw new Error('Timed out waiting for the local fixture server.')
}
