"""Composition root: the only module that knows every concrete adapter.

Run with: uvicorn --factory agent_runtime.main:create_app
"""

import logging
from contextlib import asynccontextmanager

from fastapi import FastAPI

from agent_runtime.adapters.inbound.http_api import build_router, install_error_handlers
from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.adapters.outbound.tavily import TavilySearch
from agent_runtime.adapters.outbound.vllm import VLLMModel
from agent_runtime.application import (
    ChatService,
    ChatSettings,
    ConversationStore,
    ModelPort,
    SearchPort,
)
from agent_runtime.config import Settings
from agent_runtime.domain import GenerationParams

log = logging.getLogger("agent_runtime")


def create_app(
    settings: Settings | None = None,
    *,
    model: ModelPort | None = None,
    store: ConversationStore | None = None,
    search: SearchPort | None = None,
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
    )

    @asynccontextmanager
    async def lifespan(_: FastAPI):
        log.info(
            "agent runtime started (model=%s, guardrails=%s, search=%s)",
            settings.model_name,
            settings.guardrails_enabled,
            "tavily" if search else "off",
        )
        yield
        await model.aclose()
        if search:
            await search.aclose()

    app = FastAPI(title="TatarBot Agent Runtime", version="0.1.0", lifespan=lifespan)
    install_error_handlers(app)
    app.include_router(build_router(chat_service, model, settings.agent_api_key))
    return app
