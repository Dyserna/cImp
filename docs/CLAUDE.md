# TTS-Aware Output

Your output is being processed by a wrapper application that extracts text-to-speech segments and plays them through a TTS engine. You are running in normal interactive mode with full tool access — this file does not change your behavior except in how you mark content for speech.

## TTS Markup

Wrap content that should be spoken aloud in double-bracket tags:

[[TTS]]This text will be read aloud.[[/TTS]]

The wrapper strips these tags before display, so the user does not see them in the terminal — they only hear the contents spoken.

## What to Wrap

Wrap the conversational, prose portions of your responses — the parts a person would naturally want to hear:

- Explanations, answers to questions, discussion
- Commentary on what you're about to do or what you found
- Confirmations and acknowledgments

## What NOT to Wrap

Do not wrap content that would be awkward, unhelpful, or noisy when spoken:

- Code blocks and inline code
- Command-line examples and shell output
- File paths, URLs, identifiers, hashes
- Tables, structured data, JSON
- Tool invocations and tool results
- Long lists of technical items
- Mathematical notation, formulas
- Diff output, log output

## Markup Rules

- Wrap complete sentences. Do not split a sentence across tags.
- For a response that is entirely conversational prose, one pair of tags around the whole thing is fine.
- For mixed responses (prose plus code or technical content), wrap each prose section separately, leaving the technical content outside the tags.
- Tags must not appear inside code blocks. If you need to discuss code that contains the literal characters `[[TTS]]`, that is exceedingly unlikely to come up — but if it does, just describe the content rather than including it verbatim.
- Phrase wrapped content for the ear: complete sentences, natural cadence, avoid heavy parenthetical asides, spell out things that would confuse a TTS engine when it matters (e.g., say "version two" rather than "v2" if the distinction matters).

## When TTS Markup Is Optional

If a response is entirely technical (pure code output, file edits, command execution with no prose explanation), you can omit TTS tags entirely. The wrapper will fall back to silence rather than reading technical content aloud, which is the desired behavior.

## What This File Does Not Change

Everything else about your behavior is unchanged. Use tools normally, edit files normally, work on coding tasks normally. The user has full Claude Code available — TTS is an additive layer for the conversational portions of your output, not a mode shift.
