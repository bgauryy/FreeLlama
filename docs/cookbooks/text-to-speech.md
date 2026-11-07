# Speak text aloud

**Use when** the user wants to hear a result. It does not save tokens: it is for the person, not for you.

**Model:** `phonon:en` (0.1 GB, Apple silicon, Ollama 0.40.9 or later). See `models {view:"catalog", job:"text-to-speech"}` for rivals and languages.

```json
{"tool": "audio", "arguments": {"action": "speak", "model": "phonon:en", "text": "Tests passed. The release is ready.", "outputFile": "reply.wav"}}
```

Without `outputFile`, the WAV goes to the temp directory. The result is its path and length, never the audio. Input is limited to 4,096 characters; omit `voice` for the default.

On macOS, play it with `afplay reply.wav`.

**Check it:** listen once. The score in the catalog measures how well a machine transcribes the audio, not how natural it sounds.
