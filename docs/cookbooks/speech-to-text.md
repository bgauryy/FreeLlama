# Turn a recording into text

**Use when** a WAV recording needs searchable text or a translation.

**Model:** `lfm3-audio:2b-q4_K_M` (1.7 GB), or `lfm3-omni:2b-q4_K_M` (2.0 GB), which also does images.

```json
{"tool": "audio", "arguments": {"action": "transcribe", "model": "lfm3-audio:2b-q4_K_M", "file": "recordings/standup.wav", "language": "en"}}
```

Translate into another language:

```json
{"tool": "audio", "arguments": {"action": "translate", "model": "lfm3-audio:2b-q4_K_M", "file": "recordings/standup.wav", "toLanguage": "fr"}}
```

The input must be WAV; convert other formats first (`ffmpeg -i a.mp3 a.wav`). Ollama splits long audio at pauses.

**Check it:** numbers and names are the usual misses. Measured here on generated speech only (1.7% word error); expect more errors on real recordings.
