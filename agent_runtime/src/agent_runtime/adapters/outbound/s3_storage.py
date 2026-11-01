"""`ObjectStorage` adapter for a private S3 bucket (boto3; RustFS/MinIO locally).

Credentials and region come from the standard AWS chain (the ECS task role in AWS; never
configuration); AWS_ENDPOINT_URL_S3 points boto3 at a local stand-in. Uploads use a
presigned POST because its policy can enforce a size range, which a presigned PUT cannot.
boto3 is synchronous, so network calls run in worker threads.

The `version` of an object is its S3 version id when the bucket is versioned, else
`etag:<ETag>`: the object is then read with If-Match, so a later overwrite of the same key
is detected instead of being ingested under the wrong job.
"""

import asyncio
import logging
from datetime import UTC, datetime, timedelta

import boto3
from botocore.config import Config
from botocore.exceptions import BotoCoreError, ClientError

from agent_runtime.application.ports import ObjectStorage
from agent_runtime.domain import InvalidDocumentError, ObjectMissingError, StorageError
from agent_runtime.domain.ingestion import PresignedUpload, StoredObject

log = logging.getLogger(__name__)

ETAG_PREFIX = "etag:"
_MISSING = {"404", "NoSuchKey", "NoSuchVersion", "NotFound", "412", "PreconditionFailed"}


class S3Storage(ObjectStorage):
    def __init__(self, bucket: str, client=None):
        self._bucket = bucket
        self._client = client or boto3.client(
            "s3",
            config=Config(
                signature_version="s3v4",
                retries={"mode": "standard", "max_attempts": 3},
                connect_timeout=5,
                read_timeout=30,
            ),
        )

    @property
    def bucket(self) -> str:
        return self._bucket

    def presign_upload(self, key: str, max_bytes: int, expires_seconds: int) -> PresignedUpload:
        # Signing is local: no network call, no thread needed.
        post = self._client.generate_presigned_post(
            Bucket=self._bucket,
            Key=key,
            Conditions=[["content-length-range", 1, max_bytes]],
            ExpiresIn=expires_seconds,
        )
        return PresignedUpload(
            url=post["url"],
            fields=dict(post["fields"]),
            expires_at=datetime.now(UTC) + timedelta(seconds=expires_seconds),
        )

    async def read(self, key: str, version: str | None, max_bytes: int) -> StoredObject:
        return await asyncio.to_thread(self._read, key, version, max_bytes)

    async def delete(self, key: str) -> None:
        await asyncio.to_thread(
            self._call, self._client.delete_object, Bucket=self._bucket, Key=key
        )

    def _read(self, key: str, version: str | None, max_bytes: int) -> StoredObject:
        selector = {}
        if version and version.startswith(ETAG_PREFIX):
            selector["IfMatch"] = version.removeprefix(ETAG_PREFIX)
        elif version:
            selector["VersionId"] = version
        head = self._call(self._client.head_object, Bucket=self._bucket, Key=key, **selector)
        if head["ContentLength"] > max_bytes:
            raise InvalidDocumentError(f"the file is larger than {max_bytes} bytes")
        response = self._call(self._client.get_object, Bucket=self._bucket, Key=key, **selector)
        try:
            # One byte more than allowed tells an object that grew since the HEAD.
            data = response["Body"].read(max_bytes + 1)
        except (BotoCoreError, OSError) as e:
            raise StorageError(f"S3 read failed: {type(e).__name__}") from None
        finally:
            response["Body"].close()
        if len(data) > max_bytes:
            raise InvalidDocumentError(f"the file is larger than {max_bytes} bytes")
        return StoredObject(data, response.get("VersionId"))

    @staticmethod
    def _call(method, **kwargs):
        try:
            return method(**kwargs)
        except ClientError as e:
            code = str(e.response.get("Error", {}).get("Code", ""))
            status = str(e.response.get("ResponseMetadata", {}).get("HTTPStatusCode", ""))
            if code in _MISSING or status in _MISSING:
                raise ObjectMissingError(
                    f"S3 object not found or changed ({code or status})"
                ) from None
            raise StorageError(f"S3 error {code or status}") from None
        except BotoCoreError as e:
            raise StorageError(f"S3 unavailable: {type(e).__name__}") from None
