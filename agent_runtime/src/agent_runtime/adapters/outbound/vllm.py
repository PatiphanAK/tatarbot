"""`ModelPort` adapter for vLLM's OpenAI-compatible Chat Completions API."""

import logging
import re
from typing import Any

import httpx

from agent_runtime.application.ports import ModelPort
from agent_runtime.domain import (
    InferenceRequest,
    InferenceResult,
    ModelRequestRejectedError,
    ModelResponseError,
    ModelTimeoutError,
    ModelUnavailableError,
    TokenUsage,
)

log = logging.getLogger(__name__)

_THINK_BLOCK = re.compile(r"<think>.*?</think>", re.DOTALL)
READY_TIMEOUT_SECONDS = 3.0


class VLLMModel(ModelPort):
    def __init__(
        self,
        base_url: str,
        model_name: str,
        *,
        api_key: str | None = None,
        timeout_seconds: float = 100.0,
        enable_thinking: bool | None = False,
        client: httpx.AsyncClient | None = None,
    ):
        """`base_url` is the API root, e.g. `https://host/v1`.

        `enable_thinking` is passed to the chat template (Qwen3-style
        reasoning models); `None` leaves the template default.
        """
        headers = {"Authorization": f"Bearer {api_key}"} if api_key else {}
        self._client = client or httpx.AsyncClient(
            base_url=base_url.rstrip("/") + "/",
            headers=headers,
            timeout=timeout_seconds,
        )
        self._model_name = model_name
        self._enable_thinking = enable_thinking

    async def generate(self, request: InferenceRequest) -> InferenceResult:
        try:
            response = await self._client.post("chat/completions", json=self._to_body(request))
        except httpx.TimeoutException as e:
            raise ModelTimeoutError("model request timed out") from e
        except httpx.TransportError as e:
            raise ModelUnavailableError(f"model unreachable: {type(e).__name__}") from e

        if response.status_code != 200:
            raise self._status_error(response, request.request_id)
        return self._parse(response)

    async def is_ready(self) -> bool:
        try:
            response = await self._client.get("models", timeout=READY_TIMEOUT_SECONDS)
            response.raise_for_status()
            served = {m.get("id") for m in response.json().get("data", [])}
        except (httpx.HTTPError, ValueError, AttributeError):
            return False
        return self._model_name in served

    async def aclose(self) -> None:
        await self._client.aclose()

    def _to_body(self, request: InferenceRequest) -> dict[str, Any]:
        body: dict[str, Any] = {
            "model": self._model_name,
            "messages": [{"role": m.role.value, "content": m.content} for m in request.messages],
            "stream": False,
        }
        if request.params.max_tokens is not None:
            body["max_tokens"] = request.params.max_tokens
        if request.params.temperature is not None:
            body["temperature"] = request.params.temperature
        if self._enable_thinking is not None:
            body["chat_template_kwargs"] = {"enable_thinking": self._enable_thinking}
        return body

    @staticmethod
    def _status_error(response: httpx.Response, request_id: str) -> Exception:
        status = response.status_code
        # Upstream bodies may be large or sensitive; log a prefix, never return it.
        log.warning(
            "model returned HTTP %s (request_id=%s): %.500s", status, request_id, response.text
        )
        if status in (408, 504):
            return ModelTimeoutError(f"model timed out (HTTP {status})")
        if status == 429 or status >= 500:
            return ModelUnavailableError(f"model unavailable (HTTP {status})")
        return ModelRequestRejectedError(f"model rejected request (HTTP {status})")

    @staticmethod
    def _parse(response: httpx.Response) -> InferenceResult:
        try:
            data = response.json()
            choice = data["choices"][0]
            content = choice["message"]["content"]
        except (ValueError, KeyError, IndexError, TypeError) as e:
            raise ModelResponseError("malformed model response") from e
        if not isinstance(content, str):
            raise ModelResponseError("model response has no text content")

        usage = data.get("usage") or {}
        token_usage = None
        if isinstance(usage.get("prompt_tokens"), int) and isinstance(
            usage.get("completion_tokens"), int
        ):
            token_usage = TokenUsage(usage["prompt_tokens"], usage["completion_tokens"])

        return InferenceResult(
            content=strip_reasoning(content),
            finish_reason=choice.get("finish_reason"),
            usage=token_usage,
        )


def strip_reasoning(content: str) -> str:
    """Remove `<think>...</think>` blocks, including an unterminated one."""
    content = _THINK_BLOCK.sub("", content)
    start = content.find("<think>")
    if start != -1:
        content = content[:start]
    return content.strip()
