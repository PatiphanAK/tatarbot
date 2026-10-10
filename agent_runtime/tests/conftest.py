import pytest

from agent_runtime.application import ModelPort
from agent_runtime.config import Settings
from agent_runtime.domain import InferenceRequest, InferenceResult, Language
from agent_runtime.domain.language import language_directive

API_KEY = "test-agent-key"
SYSTEM_PROMPT = "You are a test assistant."
# The system message the model receives for an English turn.
SYSTEM_EN = f"{SYSTEM_PROMPT}\n\n{language_directive(Language.ENGLISH)}"


@pytest.fixture
def anyio_backend():
    return "asyncio"


class FakeModel(ModelPort):
    """Records every InferenceRequest; replies from a script (str, InferenceResult or an
    exception to raise) or echoes."""

    def __init__(self, replies=None, ready=True):
        self.requests: list[InferenceRequest] = []
        self._replies = list(replies or [])
        self._ready = ready

    async def generate(self, request: InferenceRequest) -> InferenceResult:
        self.requests.append(request)
        if self._replies:
            reply = self._replies.pop(0)
            if isinstance(reply, Exception):
                raise reply
            if isinstance(reply, InferenceResult):
                return reply
            return InferenceResult(content=reply)
        return InferenceResult(content=f"reply to: {request.messages[-1].content}")

    async def is_ready(self) -> bool:
        return self._ready


@pytest.fixture
def settings() -> Settings:
    return Settings(
        agent_api_key=API_KEY,
        model_base_url="http://model.invalid/v1",
        model_name="test-model",
        model_api_key="model-secret",
        system_prompt=SYSTEM_PROMPT,
        max_history_messages=20,
    )
