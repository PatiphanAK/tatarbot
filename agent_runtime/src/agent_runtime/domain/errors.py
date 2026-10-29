class InvalidInputError(ValueError):
    """The caller sent something the runtime cannot process."""


class ModelError(Exception):
    """Base class for failures of the inference backend."""


class ModelTimeoutError(ModelError):
    """The backend did not answer in time."""


class ModelUnavailableError(ModelError):
    """The backend is unreachable, overloaded, or failing (5xx, 429)."""


class ModelRequestRejectedError(ModelError):
    """The backend refused the request (4xx): misconfiguration or bad input."""


class ModelResponseError(ModelError):
    """The backend answered with something unusable (malformed or empty)."""


class SearchError(Exception):
    """The search provider failed. The agent reports it to the model; the turn goes on."""


class EmbeddingError(Exception):
    """The embedding backend failed or answered with unusable vectors. Retrieval is skipped
    for that turn; a reload fails with 503."""
