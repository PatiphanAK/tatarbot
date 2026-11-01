"""Inbound queue adapter: long-polls the ingestion SQS queue and hands each message to the
`Ingestor`.

Messages are S3 event notifications (delivered straight from S3 to SQS, not through SNS)
or the runtime's own reprocess requests. A message is deleted when it is done, including
malformed ones (logged; retrying cannot fix them); otherwise its visibility is extended
with exponential backoff and SQS redelivers it. Configure a dead-letter queue on the
queue (maxReceiveCount above RAG_INGEST_MAX_ATTEMPTS) for anything that keeps failing.
"""

import asyncio
import json
import logging
from urllib.parse import unquote_plus

from agent_runtime.adapters.outbound.s3_storage import ETAG_PREFIX
from agent_runtime.application import Ingestor, Outcome
from agent_runtime.application.ingestion import REPROCESS_MESSAGE

log = logging.getLogger(__name__)

_MAX_BACKOFF_SECONDS = 900


class SqsWorker:
    def __init__(self, client, queue_url: str, ingestor: Ingestor, *, batch_size: int = 5):
        self._client = client
        self._queue_url = queue_url
        self._ingestor = ingestor
        self._batch_size = batch_size

    async def run(self, stop: asyncio.Event) -> None:
        log.info("ingestion worker polling %s", self._queue_url)
        while not stop.is_set():
            try:
                response = await asyncio.to_thread(
                    self._client.receive_message,
                    QueueUrl=self._queue_url,
                    MaxNumberOfMessages=self._batch_size,
                    WaitTimeSeconds=20,
                    MessageSystemAttributeNames=["ApproximateReceiveCount"],
                )
            except Exception as e:
                log.warning("SQS receive failed: %s; retrying in 5 s", e)
                await asyncio.sleep(5)
                continue
            for message in response.get("Messages", []):
                await self.process(message)

    async def process(self, message: dict) -> Outcome:
        try:
            outcome = await self.handle(message["Body"])
        except Exception:
            # The database or the bucket is down, or a bug: try again later.
            log.exception("ingestion message failed")
            outcome = Outcome.RETRY
        try:
            if outcome is Outcome.DONE:
                await asyncio.to_thread(
                    self._client.delete_message,
                    QueueUrl=self._queue_url,
                    ReceiptHandle=message["ReceiptHandle"],
                )
            else:
                receives = int(message.get("Attributes", {}).get("ApproximateReceiveCount", 1))
                await asyncio.to_thread(
                    self._client.change_message_visibility,
                    QueueUrl=self._queue_url,
                    ReceiptHandle=message["ReceiptHandle"],
                    VisibilityTimeout=min(30 * 2 ** (receives - 1), _MAX_BACKOFF_SECONDS),
                )
        except Exception as e:
            # The message comes back after its visibility timeout; jobs are idempotent.
            log.warning("SQS acknowledge failed: %s", e)
        return outcome

    async def handle(self, body: str) -> Outcome:
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            log.error("dropping malformed ingestion message")
            return Outcome.DONE
        if not isinstance(payload, dict):
            log.error("dropping malformed ingestion message")
            return Outcome.DONE
        if payload.get("Event") == "s3:TestEvent":
            return Outcome.DONE
        if payload.get("type") == REPROCESS_MESSAGE:
            document_id = payload.get("document_id")
            if not isinstance(document_id, int):
                log.error("dropping reprocess message without a document id")
                return Outcome.DONE
            return await self._ingestor.reprocess(document_id, bool(payload.get("force")))
        records = payload.get("Records")
        if not isinstance(records, list):
            log.error("dropping unrecognized ingestion message")
            return Outcome.DONE
        outcome = Outcome.DONE
        for record in records:
            if await self._handle_record(record) is Outcome.RETRY:
                outcome = Outcome.RETRY
        return outcome

    async def _handle_record(self, record) -> Outcome:
        try:
            if record.get("eventSource") != "aws:s3" or not str(
                record.get("eventName", "")
            ).startswith("ObjectCreated:"):
                return Outcome.DONE
            bucket = record["s3"]["bucket"]["name"]
            obj = record["s3"]["object"]
            # Keys in S3 events are URL-encoded, spaces as '+'.
            key = unquote_plus(obj["key"])
            etag = obj.get("eTag")
            version = obj.get("versionId") or (f"{ETAG_PREFIX}{etag}" if etag else "")
        except (AttributeError, KeyError, TypeError):
            log.error("dropping malformed S3 event record")
            return Outcome.DONE
        return await self._ingestor.handle_object_created(bucket, key, version)
