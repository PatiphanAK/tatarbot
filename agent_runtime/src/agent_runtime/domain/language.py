"""Reply-language policy: answer Thai in Thai and English in English.

Detection is by script, not a model: Thai characters vs Latin letters. Thai speakers mix
in English words ("ช่วยอธิบาย Docker หน่อย"), so Thai wins ties; a message that is mostly
English with one Thai word stays English. Anything else (other scripts, only emoji or
digits) is undetermined and the caller falls back to the conversation's language.
"""

from enum import StrEnum


class Language(StrEnum):
    THAI = "th"
    ENGLISH = "en"


_DIRECTIVES = {
    Language.THAI: (
        "The user is writing in Thai. Reply in Thai (ภาษาไทย), in natural, polite Thai. "
        "Keep code, commands, URLs and product names unchanged."
    ),
    Language.ENGLISH: "The user is writing in English. Reply in English.",
}


def _is_thai(ch: str) -> bool:
    return "ก" <= ch <= "๛"


def detect_language(text: str) -> Language | None:
    thai = latin = other = 0
    for ch in text:
        if _is_thai(ch):
            thai += 1
        elif ch.isascii():
            latin += ch.isalpha()
        elif ch.isalpha():
            other += 1
    if other > thai + latin or thai + latin == 0:
        return None
    return Language.THAI if thai >= latin else Language.ENGLISH


def contains_thai(text: str) -> bool:
    return any(_is_thai(ch) for ch in text)


def language_directive(language: Language) -> str:
    return _DIRECTIVES[language]
