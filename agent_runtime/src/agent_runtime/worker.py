"""Ingestion worker: the composition root for `python -m agent_runtime.worker`.

Reads the same environment as the runtime (RAG_STORE=postgres, DATABASE_URL,
RAG_S3_BUCKET, RAG_SQS_QUEUE_URL, EMBEDDING_*, RAG_CHUNK_*) and must use the same embedding
and chunking settings, or its chunks are invisible to retrieval (which filters by
embedding model). Stops after the current message on SIGTERM/SIGINT.
"""

import asyncio
import logging
import signal
import sys

import boto3

from agent_runtime.adapters.inbound.sqs_worker import SqsWorker
from agent_runtime.adapters.outbound.postgres_rag import PostgresDocumentRepository, create_pool
from agent_runtime.adapters.outbound.s3_storage import S3Storage
from agent_runtime.application import Ingestor
from agent_runtime.config import RAG_SCHEMA_DIMENSIONS, ConfigError, Settings
from agent_runtime.main import build_embedder, knowledge_settings, upload_settings

log = logging.getLogger("agent_runtime.worker")


async def run(settings: Settings) -> None:
    embedder = build_embedder(settings)
    pool = create_pool(settings.database_url, settings.database_pool_size)
    await pool.open(wait=True, timeout=30)
    try:
        ingestor = Ingestor(
            PostgresDocumentRepository(pool, RAG_SCHEMA_DIMENSIONS),
            S3Storage(settings.rag_s3_bucket),
            embedder,
            knowledge_settings(settings),
            upload_settings(settings),
        )
        worker = SqsWorker(boto3.client("sqs"), settings.rag_sqs_queue_url, ingestor)
        stop = asyncio.Event()
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, stop.set)
        log.info("ingestion worker started (embedder=%s)", embedder.identity)
        await worker.run(stop)
    finally:
        await embedder.aclose()
        await pool.close()


def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s:     %(name)s: %(message)s")
    try:
        settings = Settings.from_env()
        if not settings.uploads_enabled:
            raise ConfigError("the worker needs RAG_STORE=postgres and RAG_S3_BUCKET")
    except ConfigError as e:
        log.error("%s", e)
        return 2
    asyncio.run(run(settings))
    return 0


if __name__ == "__main__":
    sys.exit(main())
