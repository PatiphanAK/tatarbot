import os
from dataclasses import dataclass, field

DEFAULT_SYSTEM_PROMPT = "You are a helpful, friendly assistant. Answer concisely and accurately."


class ConfigError(Exception):
    pass


@dataclass(frozen=True)
class Settings:
    agent_api_key: str = field(repr=False)
    model_base_url: str
    model_name: str
    model_api_key: str | None = field(default=None, repr=False)
    # Keep below the caller's timeout (TatarBot: 120s) so callers get a 504.
    model_timeout_seconds: float = 100.0
    model_max_tokens: int | None = 1024
    model_temperature: float | None = None
    model_enable_thinking: bool | None = False
    system_prompt: str = DEFAULT_SYSTEM_PROMPT
    max_history_messages: int = 20
    max_conversations: int = 10_000

    @classmethod
    def from_env(cls) -> "Settings":
        return cls(
            agent_api_key=_required("AGENT_API_KEY"),
            model_base_url=_required("MODEL_BASE_URL"),
            model_name=_required("MODEL_NAME"),
            model_api_key=_optional("MODEL_API_KEY"),
            model_timeout_seconds=_parse("MODEL_TIMEOUT_SECONDS", float, 100.0),
            model_max_tokens=_parse("MODEL_MAX_TOKENS", int, 1024),
            model_temperature=_parse("MODEL_TEMPERATURE", float, None),
            model_enable_thinking=_parse("MODEL_ENABLE_THINKING", _bool, False),
            system_prompt=_optional("SYSTEM_PROMPT") or DEFAULT_SYSTEM_PROMPT,
            max_history_messages=_parse("MAX_HISTORY_MESSAGES", int, 20),
            max_conversations=_parse("MAX_CONVERSATIONS", int, 10_000),
        )


def _optional(key: str) -> str | None:
    value = os.environ.get(key, "").strip()
    return value or None


def _required(key: str) -> str:
    value = _optional(key)
    if value is None:
        raise ConfigError(f"missing required environment variable {key}")
    return value


def _parse(key, parse, default):
    raw = _optional(key)
    if raw is None:
        return default
    try:
        return parse(raw)
    except ValueError as e:
        raise ConfigError(f"invalid value for {key}: {e}") from None


def _bool(raw: str) -> bool:
    match raw.lower():
        case "1" | "true" | "yes" | "on":
            return True
        case "0" | "false" | "no" | "off":
            return False
    raise ValueError(f"expected true/false, got {raw!r}")
