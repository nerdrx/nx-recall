# Corrected-name assistance

Recall can optionally use trusted spellings as acoustic hints for future transcripts. Enable **Use corrected names to help recognition** in the vocabulary settings. This is experimental and defaults off.

It is not model-weight training. The normal recognizer still produces the baseline transcript. A local refinement pass may substitute one trusted name for one to three words, but only if every other recognized word agrees. The accepted change preserves the rest of the original text. “No no no” is not added as a wake phrase.

Explicit glossary names can be used directly. Future transcript corrections can add a name when the spelling matches an already named person. Speaker labels identify people; changing a speaker label alone does not train the speech decoder. Only short alphabetic ASCII names are currently eligible, with at most eight active hints. Ordinary corrected words are not automatically learned as names.

The refinement uses the selected recognition weights through the optional local voice Python runtime. It runs in an isolated helper because the bundled native recognition library does not support the required decoding mode. Private local IPC carries bounded audio and text; no cloud inference or account credentials are involved. Missing runtime, unsupported input, helper failure or timeout keeps the original recognized text.

The feature adds a second recognition pass to eligible clips of at most 15 seconds, so it uses additional CPU and memory. Disable it if the extra processing is not useful on your machine. It does not reprocess or rewrite old transcripts automatically.

A small development check used six difficult corrected “Lanalu” clips and 29 negative/control clips. Conservative name-only refinement recovered the name in two target clips and left the controls unchanged. These selected samples are preliminary evidence, not an accuracy guarantee for other people, names, accents or recordings.
