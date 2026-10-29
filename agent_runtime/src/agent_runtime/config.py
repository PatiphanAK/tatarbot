import os
from dataclasses import dataclass, field
from importlib import resources
from pathlib import Path


def default_system_prompt() -> str:
    """TatarBot persona and safety rules, shipped with the package."""
    return resources.files("agent_runtime").joinpath("prompts/system.md").read_text("utf-8")


class ConfigError(Exception):
    pass


# EMBEDDING_PROVIDER values; main.py maps each to an `EmbeddingPort` adapter.
EMBEDDING_PROVIDERS = ("openai", "hash")


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
    # Knowledge base (RAG) is enabled when a documents directory is set.
    rag_docs_dir: str | None = None
    rag_cache_file: str | None = None
    rag_chunk_chars: int = 1200
    rag_chunk_overlap: int = 200
    rag_top_k: int = 4
    rag_min_score: float = 0.3
    rag_max_context_chars: int = 4000
    rag_timeout_seconds: float = 10.0
    # Embedding model: "openai" (any OpenAI-compatible /embeddings API) or "hash" (offline).
    embedding_provider: str = "openai"
    embedding_base_url: str | None = None
    embedding_model: str | None = None
    embedding_api_key: str | None = field(default=None, repr=False)
    embedding_dimensions: int | None = None
    embedding_query_prefix: str = ""
    embedding_document_prefix: str = ""
    embedding_batch_size: int = 32
    embedding_timeout_seconds: float = 30.0

    def __post_init__(self):
        if not self.rag_docs_dir:
            return
        if self.embedding_provider not in EMBEDDING_PROVIDERS:
            raise ConfigError(
                f"EMBEDDING_PROVIDER must be one of {', '.join(EMBEDDING_PROVIDERS)}, "
                f"got {self.embedding_provider!r}"
            )
        if self.embedding_provider == "openai" and not self.embedding_model:
            raise ConfigError("EMBEDDING_MODEL is required when RAG_DOCS_DIR is set")
        if not 0 <= self.rag_chunk_overlap < self.rag_chunk_chars // 2:
            raise ConfigError("RAG_CHUNK_OVERLAP must be below half of RAG_CHUNK_CHARS")
        if self.rag_chunk_chars < 100:
            raise ConfigError("RAG_CHUNK_CHARS must be at least 100")

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
            rag_docs_dir=_optional("RAG_DOCS_DIR"),
            rag_cache_file=_optional("RAG_CACHE_FILE"),
            rag_chunk_chars=_parse("RAG_CHUNK_CHARS", int, 1200),
            rag_chunk_overlap=_parse("RAG_CHUNK_OVERLAP", int, 200),
            rag_top_k=_parse("RAG_TOP_K", int, 4),
            rag_min_score=_parse("RAG_MIN_SCORE", float, 0.3),
            rag_max_context_chars=_parse("RAG_MAX_CONTEXT_CHARS", int, 4000),
            rag_timeout_seconds=_parse("RAG_TIMEOUT_SECONDS", float, 10.0),
            **_embedding_from_env(),
        )


def _embedding_from_env() -> dict:
    """The embedding backend defaults to the chat model's server. Its key defaults to
    MODEL_API_KEY only then: a key is never sent to a host it was not configured for."""
    base_url = _optional("EMBEDDING_BASE_URL")
    api_key = _optional("EMBEDDING_API_KEY")
    if base_url is None:
        base_url = _optional("MODEL_BASE_URL")
        api_key = api_key or _optional("MODEL_API_KEY")
    return {
        "embedding_provider": (_optional("EMBEDDING_PROVIDER") or "openai").lower(),
        "embedding_base_url": base_url,
        "embedding_model": _optional("EMBEDDING_MODEL"),
        "embedding_api_key": api_key,
        "embedding_dimensions": _parse("EMBEDDING_DIMENSIONS", int, None),
        # Prefixes may end in a space or newline, so they are not stripped (\n is a newline).
        "embedding_query_prefix": _raw("EMBEDDING_QUERY_PREFIX"),
        "embedding_document_prefix": _raw("EMBEDDING_DOCUMENT_PREFIX"),
        "embedding_batch_size": _parse("EMBEDDING_BATCH_SIZE", int, 32),
        "embedding_timeout_seconds": _parse("EMBEDDING_TIMEOUT_SECONDS", float, 30.0),
    }


def _raw(key: str) -> str:
    return os.environ.get(key, "").replace("\\n", "\n")


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
