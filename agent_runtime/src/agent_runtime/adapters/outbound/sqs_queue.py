"""`IngestQueue` adapter: sends reprocess requests to the ingestion SQS queue, the same
queue S3 delivers upload events to. Credentials from the standard AWS chain;
AWS_ENDPOINT_URL_SQS points at a local stand-in."""

import asyncio
import json

import boto3
from botocore.exceptions import BotoCoreError, ClientError

from agent_runtime.application.ports import IngestQueue
from agent_runtime.domain import StorageError


class SqsQueue(IngestQueue):
    def __init__(self, queue_url: str, client=None):
        self._queue_url = queue_url
        self._client = client or boto3.client("sqs")

    async def send(self, message: dict) -> None:
        try:
            await asyncio.to_thread(
                self._client.send_message, QueueUrl=self._queue_url, MessageBody=json.dumps(message)
            )
        except (BotoCoreError, ClientError) as e:
            raise StorageError(f"SQS send failed: {type(e).__name__}") from None
