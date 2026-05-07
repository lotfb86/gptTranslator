// Phase 0: mic -> gpt-realtime-translate -> speakers
// Throwaway sanity check. Proves the API works before we touch Swift.

import 'dotenv/config';
import WebSocket from 'ws';
import { spawn } from 'node:child_process';

const TARGET_LANG = process.argv[2] || 'en';
const VALID_TARGETS = ['es', 'pt', 'fr', 'ja', 'ru', 'zh', 'de', 'ko', 'hi', 'id', 'vi', 'it', 'en'];
if (!VALID_TARGETS.includes(TARGET_LANG)) {
  console.error(`Invalid target language: ${TARGET_LANG}. Pick one of: ${VALID_TARGETS.join(', ')}`);
  process.exit(1);
}

const apiKey = process.env.OPENAI_API_KEY;
if (!apiKey) {
  console.error('Missing OPENAI_API_KEY in .env');
  process.exit(1);
}

const SAMPLE_RATE = 24000;
const URL = `wss://api.openai.com/v1/realtime/translations?model=gpt-realtime-translate`;

const ws = new WebSocket(URL, {
  headers: {
    Authorization: `Bearer ${apiKey}`,
  },
});

const soxRecord = spawn('sox', [
  '-d',
  '-t', 'raw',
  '-e', 'signed-integer',
  '-b', '16',
  '-c', '1',
  '-r', String(SAMPLE_RATE),
  '-L',
  '-',
]);

const soxPlay = spawn('sox', [
  '-t', 'raw',
  '-e', 'signed-integer',
  '-b', '16',
  '-c', '1',
  '-r', String(SAMPLE_RATE),
  '-L',
  '-',
  '-d',
]);

soxRecord.stderr.on('data', (d) => {
  const s = d.toString();
  if (!s.includes('In:') && !s.includes('Out:')) process.stderr.write(`[rec] ${s}`);
});
soxPlay.stderr.on('data', (d) => {
  const s = d.toString();
  if (!s.includes('In:') && !s.includes('Out:')) process.stderr.write(`[play] ${s}`);
});

let firstAudioOutAt = null;
let firstAudioInAt = null;
let lastDeltaAt = null;
let bytesIn = 0;
let bytesOut = 0;
const startedAt = Date.now();

function elapsed() {
  return ((Date.now() - startedAt) / 1000).toFixed(1) + 's';
}

ws.on('open', () => {
  console.log(`[${elapsed()}] Connected. Sending target language: "${TARGET_LANG}"`);
  const sessionUpdate = {
    type: 'session.update',
    session: {
      audio: {
        input: {
          transcription: { model: 'gpt-realtime-whisper' },
          noise_reduction: { type: 'near_field' },
        },
        output: { language: TARGET_LANG },
      },
    },
  };
  if (process.env.DEBUG) console.log('[client→server]', JSON.stringify(sessionUpdate));
  ws.send(JSON.stringify(sessionUpdate));
});

soxRecord.stdout.on('data', (chunk) => {
  if (ws.readyState !== WebSocket.OPEN) return;
  if (!firstAudioInAt) firstAudioInAt = Date.now();
  bytesIn += chunk.length;
  ws.send(JSON.stringify({
    type: 'session.input_audio_buffer.append',
    audio: chunk.toString('base64'),
  }));
});

ws.on('message', (raw) => {
  let msg;
  try { msg = JSON.parse(raw.toString()); } catch { return; }

  switch (msg.type) {
    case 'session.created':
    case 'session.updated': {
      const lang = msg.session?.audio?.output?.language ?? '<unset>';
      const model = msg.session?.model ?? '<unset>';
      console.log(`[${elapsed()}] ${msg.type} | server target lang=${lang} | model=${model}`);
      if (process.env.DEBUG) console.log(JSON.stringify(msg.session, null, 2));
      break;
    }

    case 'response.output_audio.delta':
    case 'session.output_audio.delta':
    case 'output_audio.delta': {
      const buf = Buffer.from(msg.delta, 'base64');
      bytesOut += buf.length;
      lastDeltaAt = Date.now();
      if (!firstAudioOutAt) {
        firstAudioOutAt = Date.now();
        const lat = firstAudioInAt ? firstAudioOutAt - firstAudioInAt : null;
        console.log(`[${elapsed()}] First translated audio out (${lat}ms after first audio in)`);
      }
      soxPlay.stdin.write(buf);
      break;
    }

    case 'response.output_audio_transcript.delta':
    case 'session.output_transcript.delta':
      process.stdout.write(msg.delta || '');
      break;
    case 'response.output_audio_transcript.done':
    case 'session.output_transcript.done':
      process.stdout.write('\n');
      break;

    case 'conversation.item.input_audio_transcription.delta':
    case 'session.input_transcript.delta':
      process.stdout.write(`\x1b[90m${msg.delta || ''}\x1b[0m`);
      break;
    case 'conversation.item.input_audio_transcription.completed':
    case 'session.input_transcript.completed':
    case 'session.input_transcript.done': {
      const lang = msg.language || msg.transcript?.language;
      if (lang) console.log(`\n[${elapsed()}] Source language detected: ${lang}`);
      break;
    }

    case 'error':
      console.error(`\n[${elapsed()}] ERROR:`, JSON.stringify(msg.error, null, 2));
      break;

    default:
      if (process.env.DEBUG) console.log(`[${elapsed()}] ${msg.type}`);
  }
});

ws.on('close', (code, reason) => {
  console.log(`\n[${elapsed()}] WebSocket closed (${code}) ${reason}`);
  cleanup();
});

ws.on('error', (err) => {
  console.error(`\n[${elapsed()}] WebSocket error:`, err.message);
});

function cleanup() {
  try { soxRecord.kill('SIGTERM'); } catch {}
  try { soxPlay.stdin.end(); soxPlay.kill('SIGTERM'); } catch {}
  console.log(`[${elapsed()}] Bytes in: ${bytesIn}, bytes out: ${bytesOut}`);
  process.exit(0);
}

process.on('SIGINT', () => {
  console.log(`\n[${elapsed()}] SIGINT, closing...`);
  try { ws.close(); } catch {}
  setTimeout(cleanup, 500);
});

console.log(`Phase 0: speaking into default mic, hearing translation to "${TARGET_LANG}".`);
console.log(`Press Ctrl+C to stop.`);
console.log(`Source-lang transcripts in gray. Translated transcripts in white.\n`);
