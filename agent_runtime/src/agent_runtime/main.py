"""Composition root: the only module that knows every concrete adapter.

Run with: uvicorn --factory agent_runtime.main:create_app
"""

import asyncio
import logging
from contextlib import asynccontextmanager

from fastapi import FastAPI

from agent_runtime.adapters.inbound.http_api import build_router, install_error_handlers
from agent_runtime.adapters.outbound.embedding_cache import CachedEmbeddings
from agent_runtime.adapters.outbound.file_documents import DirectoryDocumentSource
from agent_runtime.adapters.outbound.hash_embeddings import HashEmbeddings
from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.adapters.outbound.memory_vectors import InMemoryVectorStore
from agent_runtime.adapters.outbound.openai_embeddings import OpenAIEmbeddings
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
)
from agent_runtime.config import ConfigError, Settings
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
    store = store or InMemoryConversationStore(
        max_messages_per_conversation=max(settings.max_history_messages * 2, 2),
        max_conversations=settings.max_conversations,
    )
    knowledge = None
    if settings.rag_docs_dir:
        embedder = embedder or build_embedder(settings)
        if settings.rag_cache_file:
            embedder = CachedEmbeddings(embedder, settings.rag_cache_file)
        knowledge = KnowledgeBase(
            DirectoryDocumentSource(settings.rag_docs_dir),
            embedder,
            InMemoryVectorStore(),
            KnowledgeSettings(
                chunk_chars=settings.rag_chunk_chars,
                chunk_overlap=settings.rag_chunk_overlap,
                top_k=settings.rag_top_k,
                min_score=settings.rag_min_score,
                max_context_chars=settings.rag_max_context_chars,
                timeout_seconds=settings.rag_timeout_seconds,
            ),
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
        ),
        search=search,
        knowledge=knowledge,
    )

    @asynccontextmanager
    async def lifespan(_: FastAPI):
        log.info(
            "agent runtime started (model=%s, guardrails=%s, search=%s, knowledge=%s)",
            settings.model_name,
            settings.guardrails_enabled,
            "tavily" if search else "off",
            knowledge.status.embedder if knowledge else "off",
        )
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

    app = FastAPI(title="TatarBot Agent Runtime", version="0.1.0", lifespan=lifespan)
    install_error_handlers(app)
    app.include_router(build_router(chat_service, model, settings.agent_api_key, knowledge))
    return app


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
