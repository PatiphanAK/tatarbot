"""Deterministic guardrails around the model. Pure functions, no I/O.

They complement the rules in the system prompt, which the model may not follow:

* input: obvious prompt-injection / system-prompt extraction attempts are refused
  without calling the model;
* output: replies that reproduce the system prompt are withheld, and strings that look
  like credentials are redacted.

Harmful-content moderation is left to the model and the system prompt; a classifier
(e.g. a guard model behind its own port) would be the next step.
"""

import re

from agent_runtime.domain.language import Language

_FLAGS = re.IGNORECASE | re.DOTALL

# Narrow on purpose: asking *about* prompts ("what is a system prompt?", "Android
# developer mode", "คำสั่งระบบ linux") must still reach the model.
_INJECTION_PATTERNS = [
    re.compile(p, _FLAGS)
    for p in (
        # English
        r"\b(ignore|disregard|forget|override)\b.{0,40}\b(previous|prior|above|earlier|all|"
        r"your)\b.{0,40}\b(instructions?|rules|prompts?|guidelines)\b",
        r"\b(reveal|show|print|repeat|output|display|tell me|give me|leak)\b.{0,30}\byour\s+"
        r"(system\s+|initial\s+|hidden\s+|original\s+)?(prompt|instructions)\b",
        r"\b(DAN|jailbreak)\s+mode\b",
        r"\byou\s+are\s+now\s+(DAN|unrestricted|unfiltered|jailbroken)\b",
        # Thai (no word boundaries in Thai script)
        r"(ไม่ต้องสนใจ|เพิกเฉย|ลืม|ละเว้น).{0,15}(คำสั่ง|กฎ|ข้อกำหนด|prompt)\s*"
        r"(ก่อนหน้า|ทั้งหมด|เดิม|ของคุณ|ที่ได้รับ|ของระบบ)",
        r"(บอก|แสดง|เปิดเผย|พิมพ์|ขอดู|ทวน).{0,20}(system\s*prompt|พรอมต์ระบบ|"
        r"(คำสั่ง|prompt|พรอมต์)\s*(ของคุณ|ของบอท|ตั้งต้น))",
    )
]

_SECRET_PATTERNS = [
    re.compile(p)
    for p in (
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(-----END [A-Z ]*PRIVATE KEY-----|$)",
        r"\b[MNO][A-Za-z\d_-]{23,27}\.[A-Za-z\d_-]{6}\.[A-Za-z\d_-]{27,40}\b",  # Discord
        r"\bsk-[A-Za-z\d_-]{20,}",  # OpenAI-style
        r"\b(AKIA|ASIA)[0-9A-Z]{16}\b",  # AWS access key id
        r"\brpa_[A-Za-z\d]{20,}",  # Runpod
        r"\bhf_[A-Za-z\d]{30,}",  # Hugging Face
        r"\bgh[pousr]_[A-Za-z\d]{30,}",  # GitHub
    )
]
_REDACTED = "[redacted]"

# A reply that repeats this many distinct system-prompt lines verbatim is a leak.
_LEAK_MIN_LINES = 2
_LEAK_MIN_LINE_LENGTH = 40

_REFUSALS = {
    Language.THAI: "ขอโทษนะ เรื่องนี้ช่วยไม่ได้ ถามเรื่องอื่นได้เลย",
    Language.ENGLISH: "Sorry, I can't help with that. Feel free to ask something else.",
}


def refusal(language: Language | None) -> str:
    return _REFUSALS[language or Language.ENGLISH]


def check_input(message: str) -> str | None:
    """Return why `message` is refused, or None if it may go to the model."""
    if any(p.search(message) for p in _INJECTION_PATTERNS):
        return "prompt_injection"
    return None


def leaks_prompt(reply: str, system_prompt: str) -> bool:
    reply_norm = _normalise(reply)
    lines = {
        line
        for line in (_normalise(raw.lstrip("-*# ")) for raw in system_prompt.splitlines())
        if len(line) >= _LEAK_MIN_LINE_LENGTH
    }
    return sum(line in reply_norm for line in lines) >= _LEAK_MIN_LINES


def redact_secrets(text: str) -> tuple[str, int]:
    count = 0
    for pattern in _SECRET_PATTERNS:
        text, n = pattern.subn(_REDACTED, text)
        count += n
    return text, count


def _normalise(text: str) -> str:
    return " ".join(text.lower().split())
