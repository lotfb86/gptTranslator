# gptTranslator

Live speech translation for Google Meet (and anything else using your mic) via OpenAI's `gpt-realtime-translate` model (released 2026-05-07).

End goal: a native macOS menu bar app that:

- Captures your mic, translates your voice to the other person's language, pipes it into Google Meet via a virtual mic (BlackHole).
- Captures Chrome's audio (Meet other side), translates it to English, plays it through your speakers.
- Auto-handles same-language passthrough (model intentionally goes silent when source==target).

## Status

**Phase 0** — Node.js sanity check. Mic → translate → speakers. No Meet wiring, no virtual mic.

## Setup

```bash
npm install
brew install sox          # mic capture + audio playback
cp .env.example .env      # then add your OPENAI_API_KEY
```

## Run Phase 0

```bash
npm run translate          # default: target English
npm run translate -- es    # target Spanish
```

Speak into your mic. Hear translation back through your default speakers.

## Cost

`gpt-realtime-translate` is $0.034/min of audio, billed continuously while session is open.
