import os
from dataclasses import dataclass, field
from importlib import resources
from pathlib import Path
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError


def default_system_prompt() -> str:
    """TatarBot persona and safety rules, shipped with the package."""
    return resources.files("agent_runtime").joinpath("prompts/system.md").read_text("utf-8")


class ConfigError(Exception):
    pass


# EMBEDDING_PROVIDER values; main.py maps each to an `EmbeddingPort` adapter.
EMBEDDING_PROVIDERS = ("openai", "hash")
# RAG_STORE values: in-process memory, or Postgres + pgvector (persistent, uploads).
RAG_STORES = ("memory", "postgres")
# CONVERSATION_STORE values: in-process memory (lost on restart), or Postgres (runtime schema).
CONVERSATION_STORES = ("memory", "postgres")
# rag.chunks.embedding is vector(1024) (BGE-M3). Another size needs a new migration and a
# re-embed, never a silent change; the Postgres store also checks the live schema.
RAG_SCHEMA_DIMENSIONS = 1024
# MODEL_ENABLE_THINKING values that leave `chat_template_kwargs` out of requests. An empty
# value works locally; Cloud Run may drop empty environment variables, so `omit` is explicit.
THINKING_OMIT = ("", "omit", "none")
# A turn needs at least: reasoning, the answer, and one nudge when the model calls a tool
# it was not offered.
MIN_MODEL_CALLS = 3


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
    # USD per 1M tokens, for the cost estimate in the per-turn log. Unset: no estimate.
    model_price_input_per_mtok: float | None = None
    model_price_output_per_mtok: float | None = None
    system_prompt: str = field(default_factory=default_system_prompt)
    guardrails_enabled: bool = True
    # OWASP API8: FastAPI's /docs, /redoc and /openapi.json are off unless explicitly
    # enabled (local development); production never exposes the API description.
    api_docs_enabled: bool = False
    # Web search is enabled when a Tavily key is set.
    tavily_api_key: str | None = field(default=None, repr=False)
    search_max_searches: int = 3
    search_max_results: int = 5
    search_timeout_seconds: float = 10.0
    # Whole turn incl. searches; keep below the caller's timeout (TatarBot: 120s).
    turn_timeout_seconds: float = 110.0
    # Model calls per turn, a backstop against loops (each call is billed).
    agent_max_model_calls: int = 10
    # Local date and time the model is told, e.g. for "today's news".
    agent_time_zone: str = "Asia/Bangkok"
    max_history_messages: int = 20
    max_conversations: int = 10_000
    # Where history lives. postgres keeps it across restarts and Cloud Run scale-to-zero.
    conversation_store: str = "memory"
    conversation_retention_days: int = 30
    # Knowledge base (RAG) is enabled when a documents directory is set or RAG_STORE is
    # postgres.
    rag_store: str = "memory"
    rag_docs_dir: str | None = None
    rag_cache_file: str | None = None
    rag_chunk_chars: int = 1200
    rag_chunk_overlap: int = 200
    rag_top_k: int = 4
    rag_min_score: float = 0.3
    rag_max_context_chars: int = 4000
    rag_timeout_seconds: float = 10.0
    # Postgres store: trigram similarity a keyword hit needs (it is then kept even below
    # RAG_MIN_SCORE); 1 turns keyword search off.
    rag_keyword_min_score: float = 0.25
    # One Supabase database for both workloads: the knowledge base (schema `rag`) and
    # runtime state such as history (schema `runtime`). RUNTIME_DATABASE_URL optionally
    # points the runtime schema elsewhere (e.g. another role); empty = DATABASE_URL.
    database_url: str | None = field(default=None, repr=False)
    runtime_database_url: str | None = field(default=None, repr=False)
    database_pool_size: int = 5
    # Uploads (Postgres store only): enabled when a bucket is set. S3 credentials and
    # region come from the standard AWS chain (task role); AWS_ENDPOINT_URL_S3 / _SQS
    # point at local stand-ins.
    rag_s3_bucket: str | None = None
    rag_s3_prefix: str = "rag/"
    rag_sqs_queue_url: str | None = None
    rag_upload_max_bytes: int = 1_000_000
    rag_upload_url_ttl_seconds: int = 300
    rag_ingest_max_attempts: int = 5
    # A worker holds a job this long; a crashed worker's job is taken over after it.
    rag_ingest_lease_seconds: int = 300
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

    @property
    def knowledge_enabled(self) -> bool:
        return bool(self.rag_docs_dir) or self.rag_store == "postgres"

    @property
    def uploads_enabled(self) -> bool:
        return self.rag_store == "postgres" and bool(self.rag_s3_bucket)

    @property
    def database_needed(self) -> bool:
        return self.conversation_store == "postgres" or self.rag_store == "postgres"

    @property
    def conversation_database_url(self) -> str | None:
        """Where `runtime.conversations` lives: RUNTIME_DATABASE_URL, else DATABASE_URL."""
        return self.runtime_database_url or self.database_url

    def __post_init__(self):
        self._check_agent()
        if self.conversation_store not in CONVERSATION_STORES:
            raise ConfigError(f"CONVERSATION_STORE must be one of {', '.join(CONVERSATION_STORES)}")
        if self.conversation_store == "postgres":
            if not self.conversation_database_url:
                raise ConfigError(
                    "DATABASE_URL (or RUNTIME_DATABASE_URL) is required when "
                    "CONVERSATION_STORE=postgres"
                )
            if self.database_pool_size < 1:
                raise ConfigError("DATABASE_POOL_SIZE must be at least 1")
            if self.conversation_retention_days < 1:
                raise ConfigError("CONVERSATION_RETENTION_DAYS must be at least 1")
        if self.rag_store not in RAG_STORES:
            raise ConfigError(f"RAG_STORE must be one of {', '.join(RAG_STORES)}")
        if self.rag_s3_bucket and self.rag_store != "postgres":
            raise ConfigError("RAG_S3_BUCKET needs RAG_STORE=postgres")
        if not self.knowledge_enabled:
            return
        if self.embedding_provider not in EMBEDDING_PROVIDERS:
            raise ConfigError(
                f"EMBEDDING_PROVIDER must be one of {', '.join(EMBEDDING_PROVIDERS)}, "
                f"got {self.embedding_provider!r}"
            )
        if self.embedding_provider == "openai" and not self.embedding_model:
            raise ConfigError("EMBEDDING_MODEL is required when the knowledge base is enabled")
        if not 0 <= self.rag_chunk_overlap < self.rag_chunk_chars // 2:
            raise ConfigError("RAG_CHUNK_OVERLAP must be below half of RAG_CHUNK_CHARS")
        if self.rag_chunk_chars < 100:
            raise ConfigError("RAG_CHUNK_CHARS must be at least 100")
        if self.rag_store == "postgres":
            self._check_postgres()

    def _check_postgres(self) -> None:
        if not self.database_url:
            raise ConfigError("DATABASE_URL is required when RAG_STORE=postgres")
        dimensions = self.embedding_dimensions
        if self.embedding_provider == "hash":
            dimensions = dimensions or 512
        if dimensions is not None and dimensions != RAG_SCHEMA_DIMENSIONS:
            raise ConfigError(
                f"the RAG schema stores {RAG_SCHEMA_DIMENSIONS}-dimensional vectors; "
                f"EMBEDDING_DIMENSIONS is {dimensions} (set it to {RAG_SCHEMA_DIMENSIONS}, or "
                "use a model whose native size it is, e.g. bge-m3)"
            )
        if not 0 < self.rag_keyword_min_score <= 1:
            raise ConfigError("RAG_KEYWORD_MIN_SCORE must be in (0, 1]")
        if self.database_pool_size < 1:
            raise ConfigError("DATABASE_POOL_SIZE must be at least 1")
        if not self.rag_s3_bucket:
            return
        if not self.rag_sqs_queue_url:
            raise ConfigError("RAG_SQS_QUEUE_URL is required when RAG_S3_BUCKET is set")
        if not 1 <= self.rag_upload_max_bytes <= 50_000_000:
            raise ConfigError("RAG_UPLOAD_MAX_BYTES must be in [1, 50000000]")
        if not 60 <= self.rag_upload_url_ttl_seconds <= 3600:
            raise ConfigError("RAG_UPLOAD_URL_TTL_SECONDS must be in [60, 3600]")
        if self.rag_ingest_max_attempts < 1 or self.rag_ingest_lease_seconds < 30:
            raise ConfigError(
                "RAG_INGEST_MAX_ATTEMPTS must be at least 1, RAG_INGEST_LEASE_SECONDS 30"
            )
        if self.rag_s3_prefix and not self.rag_s3_prefix.endswith("/"):
            raise ConfigError("RAG_S3_PREFIX must end with '/'")

    def _check_agent(self) -> None:
        if self.model_timeout_seconds <= 0 or self.turn_timeout_seconds <= 0:
            raise ConfigError("MODEL_TIMEOUT_SECONDS and TURN_TIMEOUT_SECONDS must be positive")
        if self.model_timeout_seconds > self.turn_timeout_seconds:
            raise ConfigError("MODEL_TIMEOUT_SECONDS must not exceed TURN_TIMEOUT_SECONDS")
        if not 0 <= self.search_max_searches <= 10:
            raise ConfigError("SEARCH_MAX_SEARCHES must be between 0 and 10")
        if self.search_max_results < 1:
            raise ConfigError("SEARCH_MAX_RESULTS must be at least 1")
        if self.agent_max_model_calls < MIN_MODEL_CALLS:
            raise ConfigError(f"AGENT_MAX_MODEL_CALLS must be at least {MIN_MODEL_CALLS}")
        for price in (self.model_price_input_per_mtok, self.model_price_output_per_mtok):
            if price is not None and price < 0:
                raise ConfigError("MODEL_PRICE_*_PER_MTOK must not be negative")
        try:
            ZoneInfo(self.agent_time_zone)
        except (ZoneInfoNotFoundError, ValueError):
            raise ConfigError(f"unknown AGENT_TIME_ZONE {self.agent_time_zone!r}") from None

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
            model_enable_thinking=_enable_thinking(),
            model_price_input_per_mtok=_parse("MODEL_PRICE_INPUT_PER_MTOK", float, None),
            model_price_output_per_mtok=_parse("MODEL_PRICE_OUTPUT_PER_MTOK", float, None),
            system_prompt=_system_prompt(),
            guardrails_enabled=_parse("GUARDRAILS_ENABLED", _bool, True),
            api_docs_enabled=_parse("API_DOCS_ENABLED", _bool, False),
            tavily_api_key=_optional("TAVILY_API_KEY"),
            search_max_searches=_parse("SEARCH_MAX_SEARCHES", int, 3),
            search_max_results=_parse("SEARCH_MAX_RESULTS", int, 5),
            search_timeout_seconds=_parse("SEARCH_TIMEOUT_SECONDS", float, 10.0),
            turn_timeout_seconds=_parse("TURN_TIMEOUT_SECONDS", float, 110.0),
            agent_max_model_calls=_parse("AGENT_MAX_MODEL_CALLS", int, 10),
            agent_time_zone=_optional("AGENT_TIME_ZONE") or "Asia/Bangkok",
            max_history_messages=_parse("MAX_HISTORY_MESSAGES", int, 20),
            max_conversations=_parse("MAX_CONVERSATIONS", int, 10_000),
            conversation_store=(_optional("CONVERSATION_STORE") or "memory").lower(),
            conversation_retention_days=_parse("CONVERSATION_RETENTION_DAYS", int, 30),
            rag_store=(_optional("RAG_STORE") or "memory").lower(),
            rag_docs_dir=_optional("RAG_DOCS_DIR"),
            rag_cache_file=_optional("RAG_CACHE_FILE"),
            rag_chunk_chars=_parse("RAG_CHUNK_CHARS", int, 1200),
            rag_chunk_overlap=_parse("RAG_CHUNK_OVERLAP", int, 200),
            rag_top_k=_parse("RAG_TOP_K", int, 4),
            rag_min_score=_parse("RAG_MIN_SCORE", float, 0.3),
            rag_max_context_chars=_parse("RAG_MAX_CONTEXT_CHARS", int, 4000),
            rag_timeout_seconds=_parse("RAG_TIMEOUT_SECONDS", float, 10.0),
            rag_keyword_min_score=_parse("RAG_KEYWORD_MIN_SCORE", float, 0.25),
            database_url=_optional("DATABASE_URL"),
            runtime_database_url=_optional("RUNTIME_DATABASE_URL"),
            database_pool_size=_parse("DATABASE_POOL_SIZE", int, 5),
            rag_s3_bucket=_optional("RAG_S3_BUCKET"),
            rag_s3_prefix=os.environ.get("RAG_S3_PREFIX", "rag/").strip(),
            rag_sqs_queue_url=_optional("RAG_SQS_QUEUE_URL"),
            rag_upload_max_bytes=_parse("RAG_UPLOAD_MAX_BYTES", int, 1_000_000),
            rag_upload_url_ttl_seconds=_parse("RAG_UPLOAD_URL_TTL_SECONDS", int, 300),
            rag_ingest_max_attempts=_parse("RAG_INGEST_MAX_ATTEMPTS", int, 5),
            rag_ingest_lease_seconds=_parse("RAG_INGEST_LEASE_SECONDS", int, 300),
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


def _enable_thinking() -> bool | None:
    """MODEL_ENABLE_THINKING. Unset: false (hide Qwen3 thinking). Empty, `omit` or `none`:
    None, so `chat_template_kwargs` is left out, for providers that reject unknown fields
    (OpenAI)."""
    if "MODEL_ENABLE_THINKING" not in os.environ:
        return False
    if os.environ["MODEL_ENABLE_THINKING"].strip().lower() in THINKING_OMIT:
        return None
    return _parse("MODEL_ENABLE_THINKING", _bool, False)


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
