"""Composition root: the only module that knows every concrete adapter.

Run with: uvicorn --factory agent_runtime.main:create_app
"""

import asyncio
import logging
from contextlib import asynccontextmanager
from urllib.parse import urlsplit

from fastapi import FastAPI
from psycopg_pool import AsyncConnectionPool

from agent_runtime.adapters.inbound.http_api import build_router, install_error_handlers
from agent_runtime.adapters.outbound.embedding_cache import CachedEmbeddings
from agent_runtime.adapters.outbound.file_documents import DirectoryDocumentSource
from agent_runtime.adapters.outbound.hash_embeddings import HashEmbeddings
from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.adapters.outbound.memory_vectors import InMemoryVectorStore
from agent_runtime.adapters.outbound.openai_embeddings import OpenAIEmbeddings
from agent_runtime.adapters.outbound.postgres_conversations import PostgresConversationStore
from agent_runtime.adapters.outbound.postgres_rag import (
    PostgresDocumentRepository,
    PostgresVectorStore,
    create_pool,
)
from agent_runtime.adapters.outbound.s3_storage import S3Storage
from agent_runtime.adapters.outbound.sqs_queue import SqsQueue
from agent_runtime.adapters.outbound.tavily import TavilySearch
from agent_runtime.adapters.outbound.vllm import VLLMModel
from agent_runtime.application import (
    ChatService,
    ChatSettings,
    ConversationStore,
    EmbeddingPort,
    KnowledgeBase,
    KnowledgeSettings,
    ModelPort,
    SearchPort,
    Uploads,
    UploadSettings,
    VectorStore,
)
from agent_runtime.config import RAG_SCHEMA_DIMENSIONS, ConfigError, Settings
from agent_runtime.domain import EmbeddingError, GenerationParams

log = logging.getLogger("agent_runtime")


def create_app(
    settings: Settings | None = None,
    *,
    model: ModelPort | None = None,
    store: ConversationStore | None = None,
    search: SearchPort | None = None,
    embedder: EmbeddingPort | None = None,
) -> FastAPI:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s:     %(name)s: %(message)s")
    settings = settings or Settings.from_env()
    model = model or VLLMModel(
        settings.model_base_url,
        settings.model_name,
        api_key=settings.model_api_key,
        timeout_seconds=settings.model_timeout_seconds,
        enable_thinking=settings.model_enable_thinking,
    )
    if search is None and settings.tavily_api_key:
        search = TavilySearch(
            settings.tavily_api_key, timeout_seconds=settings.search_timeout_seconds
        )
    # One pool per database URL: the knowledge store (schema rag) and history (schema
    # runtime) share it unless RUNTIME_DATABASE_URL points history elsewhere.
    pools: dict[str, AsyncConnectionPool] = {}

    def pool_for(url: str) -> AsyncConnectionPool:
        if url not in pools:
            pools[url] = create_pool(url, settings.database_pool_size)
        return pools[url]

    if store is None:
        history_size = max(settings.max_history_messages * 2, 2)
        if settings.conversation_store == "postgres":
            store = PostgresConversationStore(
                pool_for(settings.conversation_database_url),
                max_messages_per_conversation=history_size,
                retention_days=settings.conversation_retention_days,
            )
        else:
            store = InMemoryConversationStore(
                max_messages_per_conversation=history_size,
                max_conversations=settings.max_conversations,
            )
    knowledge = None
    uploads = None
    vectors: VectorStore | None = None
    if settings.knowledge_enabled:
        embedder = embedder or build_embedder(settings)
        if settings.rag_cache_file:
            embedder = CachedEmbeddings(embedder, settings.rag_cache_file)
        if settings.rag_store == "postgres":
            pool = pool_for(settings.database_url)
            vectors = PostgresVectorStore(
                pool,
                embedder.identity,
                RAG_SCHEMA_DIMENSIONS,
                keyword_min_score=settings.rag_keyword_min_score,
            )
            if settings.uploads_enabled:
                uploads = Uploads(
                    PostgresDocumentRepository(pool, RAG_SCHEMA_DIMENSIONS),
                    S3Storage(settings.rag_s3_bucket),
                    SqsQueue(settings.rag_sqs_queue_url),
                    upload_settings(settings),
                )
        else:
            vectors = InMemoryVectorStore()
        knowledge = KnowledgeBase(
            DirectoryDocumentSource(settings.rag_docs_dir) if settings.rag_docs_dir else None,
            embedder,
            vectors,
            knowledge_settings(settings),
        )
    chat_service = ChatService(
        model,
        store,
        ChatSettings(
            system_prompt=settings.system_prompt,
            max_history_messages=settings.max_history_messages,
            generation=GenerationParams(
                max_tokens=settings.model_max_tokens,
                temperature=settings.model_temperature,
            ),
            guardrails_enabled=settings.guardrails_enabled,
            max_searches=settings.search_max_searches,
            results_per_search=settings.search_max_results,
            turn_timeout_seconds=settings.turn_timeout_seconds,
            max_model_calls=settings.agent_max_model_calls,
            time_zone=settings.agent_time_zone,
            price_input_per_mtok=settings.model_price_input_per_mtok,
            price_output_per_mtok=settings.model_price_output_per_mtok,
        ),
        search=search,
        knowledge=knowledge,
    )

    @asynccontextmanager
    async def lifespan(_: FastAPI):
        log.info(
            "agent runtime started (model=%s, model_host=%s, model_timeout_s=%g, thinking=%s, "
            "max_model_calls=%d, max_searches=%d, time_zone=%s, pricing=%s, guardrails=%s, "
            "search=%s, knowledge=%s, store=%s, uploads=%s, history=%s)",
            settings.model_name,
            urlsplit(settings.model_base_url).hostname,
            settings.model_timeout_seconds,
            "omit" if settings.model_enable_thinking is None else settings.model_enable_thinking,
            settings.agent_max_model_calls,
            settings.search_max_searches,
            settings.agent_time_zone,
            "on"
            if None
            not in (settings.model_price_input_per_mtok, settings.model_price_output_per_mtok)
            else "off",
            settings.guardrails_enabled,
            "tavily" if search else "off",
            knowledge.status.embedder if knowledge else "off",
            settings.rag_store if knowledge else "off",
            "on" if uploads else "off",
            settings.conversation_store,
        )
        for pool in pools.values():
            # Does not wait for a connection: a database outage leaves the runtime serving
            # without knowledge or history, like an embedding outage.
            await pool.open(wait=False)
        # Indexing runs in the background: the API serves at once (without knowledge
        # until the index is built), and a cold embedding backend cannot block startup.
        indexing = asyncio.create_task(_initial_index(knowledge)) if knowledge else None
        yield
        if indexing:
            indexing.cancel()
        await model.aclose()
        if search:
            await search.aclose()
        if embedder:
            await embedder.aclose()
        for pool in pools.values():
            await pool.close()

    docs = settings.api_docs_enabled
    app = FastAPI(
        title="TatarBot Agent Runtime",
        version="0.1.0",
        lifespan=lifespan,
        # OWASP API8: no unauthenticated API description in production.
        docs_url="/docs" if docs else None,
        redoc_url="/redoc" if docs else None,
        openapi_url="/openapi.json" if docs else None,
    )
    install_error_handlers(app)
    app.include_router(
        build_router(chat_service, model, settings.agent_api_key, knowledge, uploads)
    )
    return app


def knowledge_settings(settings: Settings) -> KnowledgeSettings:
    return KnowledgeSettings(
        chunk_chars=settings.rag_chunk_chars,
        chunk_overlap=settings.rag_chunk_overlap,
        top_k=settings.rag_top_k,
        min_score=settings.rag_min_score,
        max_context_chars=settings.rag_max_context_chars,
        timeout_seconds=settings.rag_timeout_seconds,
    )


def upload_settings(settings: Settings) -> UploadSettings:
    return UploadSettings(
        max_bytes=settings.rag_upload_max_bytes,
        url_ttl_seconds=settings.rag_upload_url_ttl_seconds,
        key_prefix=settings.rag_s3_prefix,
        max_attempts=settings.rag_ingest_max_attempts,
        lease_seconds=settings.rag_ingest_lease_seconds,
    )


def build_embedder(settings: Settings) -> EmbeddingPort:
    """The embedding model named by EMBEDDING_PROVIDER. Adding a provider means one adapter
    and one case here; the pipeline does not change."""
    match settings.embedding_provider:
        case "openai":
            if not settings.embedding_base_url or not settings.embedding_model:
                raise ConfigError(
                    "EMBEDDING_BASE_URL (or MODEL_BASE_URL) and EMBEDDING_MODEL are required"
                )
            return OpenAIEmbeddings(
                settings.embedding_base_url,
                settings.embedding_model,
                api_key=settings.embedding_api_key,
                dimensions=settings.embedding_dimensions,
                query_prefix=settings.embedding_query_prefix,
                document_prefix=settings.embedding_document_prefix,
                batch_size=settings.embedding_batch_size,
                timeout_seconds=settings.embedding_timeout_seconds,
            )
        case "hash":
            return HashEmbeddings(settings.embedding_dimensions or 512)
    raise ConfigError(f"unknown EMBEDDING_PROVIDER {settings.embedding_provider!r}")


async def _initial_index(knowledge: KnowledgeBase) -> None:
    try:
        await knowledge.refresh()
    except EmbeddingError as e:
        log.error("knowledge indexing failed, retry with POST /v1/knowledge/reload: %s", e)
    except Exception:
        log.exception("knowledge indexing failed")
