import os
from dataclasses import dataclass, field
from importlib import resources
from pathlib import Path


def default_system_prompt() -> str:
    """TatarBot persona and safety rules, shipped with the package."""
    return resources.files("agent_runtime").joinpath("prompts/system.md").read_text("utf-8")


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
    system_prompt: str = field(default_factory=default_system_prompt)
    guardrails_enabled: bool = True
    # Web search is enabled when a Tavily key is set.
    tavily_api_key: str | None = field(default=None, repr=False)
    search_max_searches: int = 3
    search_max_results: int = 5
    search_timeout_seconds: float = 10.0
    # Whole turn incl. searches; keep below the caller's timeout (TatarBot: 120s).
    turn_timeout_seconds: float = 110.0
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
            system_prompt=_system_prompt(),
            guardrails_enabled=_parse("GUARDRAILS_ENABLED", _bool, True),
            tavily_api_key=_optional("TAVILY_API_KEY"),
            search_max_searches=_parse("SEARCH_MAX_SEARCHES", int, 3),
            search_max_results=_parse("SEARCH_MAX_RESULTS", int, 5),
            search_timeout_seconds=_parse("SEARCH_TIMEOUT_SECONDS", float, 10.0),
            turn_timeout_seconds=_parse("TURN_TIMEOUT_SECONDS", float, 110.0),
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


def _system_prompt() -> str:
    """SYSTEM_PROMPT (inline) > SYSTEM_PROMPT_FILE > the packaged default."""
    if inline := _optional("SYSTEM_PROMPT"):
        return inline
    if path := _optional("SYSTEM_PROMPT_FILE"):
        try:
            text = Path(path).read_text("utf-8").strip()
        except OSError as e:
            raise ConfigError(f"cannot read SYSTEM_PROMPT_FILE: {e.strerror}") from None
        if not text:
            raise ConfigError("SYSTEM_PROMPT_FILE is empty")
        return text
    return default_system_prompt()


def _bool(raw: str) -> bool:
    match raw.lower():
        case "1" | "true" | "yes" | "on":
            return True
        case "0" | "false" | "no" | "off":
            return False
    raise ValueError(f"expected true/false, got {raw!r}")
