import pytest

from agent_runtime.domain import Language, detect_language


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("สวัสดีครับ", Language.THAI),
        ("ช่วยอธิบาย Docker compose หน่อย", Language.THAI),
        ("Hello, how are you?", Language.ENGLISH),
        ("What does สวัสดี mean?", Language.ENGLISH),
        ("ok", Language.ENGLISH),
        ("555", None),
        ("👍🎉", None),
        ("こんにちは", None),
        ("", None),
    ],
)
def test_detect_language(text, expected):
    assert detect_language(text) is expected
