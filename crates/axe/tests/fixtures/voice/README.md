# Voice fixtures

`en-fleurs.ogg` and `zh-fleurs.ogg` are one utterance each from the dev split
of Google's FLEURS dataset (`en_us`, `cmn_hans_cn`;
<https://huggingface.co/datasets/google/fleurs>, CC BY 4.0), re-encoded to
16 kHz mono Ogg/Opus at 24 kbit/s like a Telegram voice note. The `.txt` files
are their FLEURS transcriptions. Used by `tests/telegram.rs` (the voice path)
and `tests/transcribe_fleurs.rs` (accuracy against the real engine).
