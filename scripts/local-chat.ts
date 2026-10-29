#!/usr/bin/env bun
/** A terminal front end to fake Slack; every message still enters Slackbot's signed webhook. */
import { createInterface, clearLine, cursorTo } from 'node:readline'
import { stripVTControlCharacters } from 'node:util'
import { BOT, TEAM, USER } from '../e2e/fakes/slack-fixture'

export const DEFAULT_URL = 'http://127.0.0.1:30443'
const REQUEST_TIMEOUT_MS = 5_000

export type Message = {
  ts: string
  text: string
  user?: string
  bot_id?: string
  thread_ts?: string
  reply_count?: number
  streaming?: boolean
  reactions?: Array<{ name: string; count: number }>
}
type Ephemeral = { channel: string; text: string; thread_ts?: string }
export type Snapshot = { messages: Message[]; ephemerals: Ephemeral[]; status: string }

/** Fixture credentials must only reach a local test server. */
export function localUrl(value: string): string {
  const url = new URL(value)
  if (!['http:', 'https:'].includes(url.protocol) || !['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname)) {
    throw new Error('The fake Slack URL must use http(s) on localhost, 127.0.0.1, or [::1]. Use a local port-forward for a cluster.')
  }
  if (url.username || url.password || url.search || url.hash || url.pathname !== '/') {
    throw new Error('The fake Slack URL must be an origin, without credentials, a path, query, or fragment.')
  }
  return url.origin
}

export class LocalChat {
  readonly url: string
  channel = ''

  constructor(url = DEFAULT_URL) {
    this.url = localUrl(url)
  }

  get sessionKey(): string {
    return `slack:${this.channel}:`
  }

  async connect(): Promise<void> {
    const response = await fetch(`${this.url}/healthz`, { signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS) })
    if (!response.ok) throw new Error(`Fake Slack health check failed (HTTP ${response.status}).`)
    const identity = await this.api<{ team_id: string; user_id: string }>('auth.test')
    if (identity.team_id !== TEAM.id || identity.user_id !== USER.id) {
      throw new Error('The server did not return the local Slack fixture identity.')
    }
    const { channel } = await this.api<{ channel: { id: string } }>('conversations.open', { users: USER.id })
    if (channel.id !== `D${USER.id.slice(1)}`) throw new Error('The server returned an unexpected fixture DM.')
    this.channel = channel.id
  }

  async send(text: string): Promise<string> {
    this.requireConnection()
    // Deliberately no thread_ts: the opted-in ingress chooses the canonical session.
    const result = await this.api<{ ts: string }>('chat.postMessage', { channel: this.channel, text })
    return result.ts
  }

  async snapshot(): Promise<Snapshot> {
    this.requireConnection()
    const query = new URLSearchParams({ channel: this.channel, thread_ts: '' })
    const [history, ephemerals, status] = await Promise.all([
      this.api<{ messages: Message[] }>('conversations.history', { channel: this.channel }),
      this.control<Ephemeral[]>(`/_e2e/ephemeral?user=${USER.id}`),
      this.control<{ status: string }>(`/_e2e/status?${query}`)
    ])
    // Also show threaded replies if the opt-in is disabled, so rollout mistakes are visible.
    const replies = await Promise.all(history.messages.filter(message => message.reply_count).map(message =>
      this.api<{ messages: Message[] }>('conversations.replies', { channel: this.channel, ts: message.ts })
    ))
    const messages = new Map(history.messages.map(message => [message.ts, message]))
    for (const reply of replies) for (const message of reply.messages) messages.set(message.ts, message)
    return {
      messages: [...messages.values()].sort((a, b) => a.ts.localeCompare(b.ts)),
      ephemerals: ephemerals.filter(message => message.channel === this.channel),
      status: status.status
    }
  }

  private requireConnection(): void {
    if (!this.channel) throw new Error('Connect to fake Slack before sending or reading messages.')
  }

  private async api<T>(method: string, body: Record<string, unknown> = {}): Promise<T> {
    const response = await fetch(`${this.url}/api/${method}`, {
      method: 'POST',
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      headers: { authorization: `Bearer ${USER.token}`, 'content-type': 'application/json' },
      body: JSON.stringify(body)
    })
    if (!response.ok) throw new Error(`${method} failed (HTTP ${response.status}).`)
    const result = await response.json() as T & { ok: boolean; error?: string }
    if (!result.ok) throw new Error(`${method} failed: ${result.error ?? 'unknown error'}.`)
    return result
  }

  private async control<T>(path: string): Promise<T> {
    const response = await fetch(`${this.url}${path}`, { signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS) })
    if (!response.ok) throw new Error(`Fake Slack control request failed (HTTP ${response.status}).`)
    return response.json() as Promise<T>
  }
}

export type Command = { kind: 'send'; text: string } | { kind: 'help' | 'status' | 'history' | 'quit' | 'empty' | 'unknown' }

export function parseInput(line: string): Command {
  const text = line.trim()
  if (!text) return { kind: 'empty' }
  if (text === '/stop') return { kind: 'send', text: 'stop' }
  if (text === '/quit' || text === '/exit') return { kind: 'quit' }
  if (text === '/help' || text === '/status' || text === '/history') return { kind: text.slice(1) as 'help' | 'status' | 'history' }
  // Escape a slash to send an actual slash command to Centaur.
  if (text.startsWith('//')) return { kind: 'send', text: text.slice(1) }
  return text.startsWith('/') ? { kind: 'unknown' } : { kind: 'send', text }
}

export function safeText(text: string): string {
  return stripVTControlCharacters(text).replace(/[\u0000-\u0008\u000b-\u001f\u007f]/g, '')
}

export class Transcript {
  private seen = new Map<string, string>()
  private ephemeralCount = 0
  private previousStatus = ''

  changes(snapshot: Snapshot): string[] {
    const output: string[] = []
    for (const message of snapshot.messages) {
      if (!isBot(message)) continue
      const signature = JSON.stringify([message.text, message.streaming])
      const previous = this.seen.get(message.ts)
      if (previous === signature) continue
      this.seen.set(message.ts, signature)
      if (!message.text || message.text === '...') continue
      const suffix = message.streaming ? ' (updating)' : previous ? ' (updated)' : ''
      output.push(`Centaur${suffix}:\n${safeText(message.text)}`)
    }
    if (snapshot.ephemerals.length < this.ephemeralCount) this.ephemeralCount = 0
    for (const message of snapshot.ephemerals.slice(this.ephemeralCount)) output.push(`Notice: ${safeText(message.text)}`)
    this.ephemeralCount = snapshot.ephemerals.length
    if (snapshot.status && snapshot.status !== this.previousStatus) output.push(`Status: ${safeText(snapshot.status)}`)
    this.previousStatus = snapshot.status
    return output
  }

  remember(snapshot: Snapshot): void {
    this.changes(snapshot)
  }
}

function isBot(message: Message): boolean {
  return message.user === BOT.userId || Boolean(message.bot_id)
}

const HELP = `Type a message and press Enter. You can send another message while Centaur is working.
/stop     Send a bare stop message through Slackbot
/status   Show connection and visible Slack activity
/history  Replay the visible chat
/help     Show commands
/quit     Disconnect; a running execution keeps going
//text    Send a message beginning with /`

function historyLines(snapshot: Snapshot): string[] {
  return snapshot.messages.filter(message => message.text && message.text !== '...').map(message =>
    `${isBot(message) ? 'Centaur' : 'You'}: ${safeText(message.text)}`
  )
}

export async function main(args = process.argv.slice(2)): Promise<void> {
  let url = process.env.CENTAUR_LOCAL_SLACK_URL ?? DEFAULT_URL
  for (let index = 0; index < args.length; index++) {
    const arg = args[index]
    if (arg === '--help' || arg === '-h') {
      console.log(`Usage: bun scripts/local-chat.ts [--url ${DEFAULT_URL}]\n\n${HELP}\n\nRequires the local stack and fake Slack. The fixture DM must be opted in to unified mode.`)
      return
    }
    if (arg === '--url' && args[index + 1]) url = args[++index]!
    else throw new Error(`Unknown or incomplete option: ${arg}`)
  }
  const chat = new LocalChat(url)
  await chat.connect()
  const transcript = new Transcript()
  const initial = await chat.snapshot()
  transcript.remember(initial)
  console.log(`Centaur local chat • ${chat.url}\nFixture ${TEAM.id}:${USER.id} • DM ${chat.channel}\nExpected unified session: ${chat.sessionKey}\n${HELP}`)
  const recent = historyLines(initial).slice(-10)
  if (recent.length) console.log(`\nRecent chat:\n${recent.join('\n\n')}\n`)
  console.log('Messages use fake Slack and the real Slackbot ingress. Model behavior depends on the local stack configuration.\n')

  const terminal = Boolean(process.stdin.isTTY && process.stdout.isTTY)
  const input = createInterface({ input: process.stdin, output: process.stdout, terminal, prompt: 'You> ' })
  let closed = false
  let inputEnded = false
  let stopRequested = false
  let pollTimer: ReturnType<typeof setTimeout> | undefined
  let commands = Promise.resolve()
  let connectionFailed = false
  const prompt = () => {
    if (closed || inputEnded) return
    if (terminal) {
      clearLine(process.stdout, 0)
      cursorTo(process.stdout, 0)
    }
    input.prompt(true)
  }
  const write = (text: string) => {
    if (closed) return
    if (terminal) {
      clearLine(process.stdout, 0)
      cursorTo(process.stdout, 0)
    }
    console.log(`${text}\n`)
    prompt()
  }
  const refresh = async (): Promise<Snapshot> => {
    const snapshot = await chat.snapshot()
    if (connectionFailed) write('Connection restored.')
    connectionFailed = false
    for (const text of transcript.changes(snapshot)) write(text)
    return snapshot
  }
  const poll = async () => {
    try { await refresh() } catch (error) {
      if (!connectionFailed) write(`Connection interrupted: ${safeText(error instanceof Error ? error.message : String(error))}\nRetrying; /quit disconnects.`)
      connectionFailed = true
    }
    if (!closed && !inputEnded) pollTimer = setTimeout(poll, 750)
  }
  input.on('line', line => {
    commands = commands.then(async () => {
      if (closed || stopRequested) return
      const command = parseInput(line)
      switch (command.kind) {
        case 'send':
          await chat.send(command.text)
          // Acceptance by fake Slack is not proof the webhook/execution completed.
          if (command.text === 'stop') write('Stop submitted to fake Slack.')
          break
        case 'help': write(HELP); break
        case 'unknown': write('Unknown command. Use /help, or // to send a leading slash.'); break
        case 'history': write(historyLines(await refresh()).join('\n\n') || 'No visible messages yet.'); break
        case 'status': {
          const snapshot = await refresh()
          const botMessages = snapshot.messages.filter(isBot)
          const threaded = botMessages.filter(message => message.thread_ts && message.thread_ts !== message.ts).length
          const reactions = snapshot.messages.at(-1)?.reactions?.map(reaction => reaction.name).join(', ')
          write(`Connected to ${chat.url}\nDM: ${chat.channel}\nExpected unified session: ${chat.sessionKey}\nVisible messages: ${snapshot.messages.length}; bot replies: ${botMessages.length}; marked streaming: ${botMessages.filter(message => message.streaming).length}\nSlack status: ${safeText(snapshot.status) || '(none)'}${reactions ? `\nLatest reactions: ${reactions}` : ''}${threaded ? `\n${threaded} threaded bot replies are visible; check the unified DM opt-in.` : ''}\nSlack visibility does not confirm execution completion; unified DMs do not use native thread status.`)
          break
        }
        case 'quit': stopRequested = true; input.close(); return
        case 'empty': break
      }
      prompt()
    }).catch(error => {
      write(`Error: ${safeText(error instanceof Error ? error.message : String(error))}\nA failed send is not retried automatically; check /history before resending.`)
    })
  })
  input.on('SIGINT', () => { stopRequested = true; input.close() })
  input.on('close', () => {
    inputEnded = true
    if (pollTimer) clearTimeout(pollTimer)
    // EOF may arrive immediately after piped input. Finish accepted commands first.
    void commands.finally(() => {
      closed = true
      if (pollTimer) clearTimeout(pollTimer)
      console.log('\nDisconnected. Any running execution continues; reconnect to see its output.')
    })
  })
  prompt()
  pollTimer = setTimeout(poll, 750)
}

if (import.meta.main) {
  main().catch(error => {
    console.error(`Local chat: ${safeText(error instanceof Error ? error.message : String(error))}\nStart the local stack and fake Slack, or choose its port with --url.`)
    process.exitCode = 1
  })
}
