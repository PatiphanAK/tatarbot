import asyncio

import pytest
from conftest import SYSTEM_PROMPT, FakeModel

from agent_runtime.adapters.outbound.file_documents import DirectoryDocumentSource
from agent_runtime.adapters.outbound.hash_embeddings import HashEmbeddings
from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.adapters.outbound.memory_vectors import InMemoryVectorStore
from agent_runtime.application import (
    ChatService,
    ChatSettings,
    DocumentSource,
    EmbeddingPort,
    KnowledgeBase,
    KnowledgeSettings,
)
from agent_runtime.domain import Chunk, Document, EmbeddingError, Role
from agent_runtime.domain.knowledge import chunk_document, document_title

pytestmark = pytest.mark.anyio

GUIDE = """\
# TatarBot guide

Intro paragraph.

## Music

### Uploads

Use /upload with an audio file up to 25 MB.

## Radio

Use /radio lofi to play the lofi station.
"""


class StaticSource(DocumentSource):
    def __init__(self, documents):
        self.documents = list(documents)

    async def load(self):
        return list(self.documents)


class KeywordEmbeddings(EmbeddingPort):
    """Deterministic 3-d space: counts of "upload", "radio", "intro"."""

    def __init__(self, fail_query=False, delay=0.0):
        self.document_calls: list[list[str]] = []
        self.fail_query = fail_query
        self.delay = delay

    @property
    def identity(self):
        return "keyword:3"

    async def embed_documents(self, texts):
        self.document_calls.append(list(texts))
        return [self._vec(t) for t in texts]

    async def embed_query(self, text):
        await asyncio.sleep(self.delay)
        if self.fail_query:
            raise EmbeddingError("down")
        return self._vec(text)

    @staticmethod
    def _vec(text):
        t = text.lower()
        return [
            float(t.count("upload")),
            float(t.count("radio")),
            float(t.count("intro")),
        ]


GUIDE_DOC = Document("guide.md", "TatarBot guide", GUIDE)


def kb(documents=(GUIDE_DOC,), embedder=None, **settings):
    return KnowledgeBase(
        StaticSource(documents),
        embedder or KeywordEmbeddings(),
        InMemoryVectorStore(),
        KnowledgeSettings(**settings),
    )


# ---- chunking


def test_chunks_follow_headings_without_repeating_the_title():
    doc = Document("guide.md", "TatarBot guide", GUIDE)
    chunks = chunk_document(doc)
    assert [(c.heading, c.text) for c in chunks] == [
        ("", "Intro paragraph."),
        ("Music > Uploads", "Use /upload with an audio file up to 25 MB."),
        ("Radio", "Use /radio lofi to play the lofi station."),
    ]
    assert [c.index for c in chunks] == [0, 1, 2]
    assert chunks[1].embedding_text == (
        "TatarBot guide > Music > Uploads\nUse /upload with an audio file up to 25 MB."
    )


def test_paragraphs_are_packed_and_long_ones_split_with_overlap():
    words = " ".join(f"w{i:03d}" for i in range(100))  # 499 chars
    doc = Document("d", "D", f"short one\n\nshort two\n\n{words}")
    chunks = chunk_document(doc, max_chars=200, overlap=40)

    assert chunks[0].text == "short one\n\nshort two"
    windows = [c.text for c in chunks[1:]]
    assert all(len(w) <= 200 for w in windows)
    assert windows[0].startswith("w000") and windows[-1].endswith("w099")
    # Consecutive windows share text.
    assert windows[0][-20:].split()[-1] in windows[1]


def test_thai_text_without_spaces_is_hard_cut():
    doc = Document("th", "TH", "ก" * 450)
    chunks = chunk_document(doc, max_chars=200, overlap=50)
    assert [len(c.text) for c in chunks] == [200, 200, 150]


def test_headings_inside_code_blocks_are_text():
    doc = Document("d", "D", "## Real\n\n```sh\n# not a heading\n```")
    [chunk] = chunk_document(doc)
    assert chunk.heading == "Real"
    assert "# not a heading" in chunk.text


def test_chunk_key_depends_on_location_and_text():
    a = Chunk("d", "T", "H", "text", 0)
    assert a.key == Chunk("other", "T", "H", "text", 5).key
    assert a.key != Chunk("d", "T", "H2", "text", 0).key


def test_document_title():
    assert document_title("intro\n# Title #\n## Sub", "file") == "Title"
    assert document_title("no headings", "file") == "file"


def test_invalid_chunk_settings():
    with pytest.raises(ValueError):
        chunk_document(Document("d", "D", "x"), max_chars=50)
    with pytest.raises(ValueError):
        chunk_document(Document("d", "D", "x"), max_chars=200, overlap=100)


# ---- pipeline


async def test_refresh_indexes_and_retrieve_ranks_by_similarity():
    knowledge = kb()
    assert await knowledge.retrieve("how do I upload?") == []  # not indexed yet

    status = await knowledge.refresh()
    assert (status.documents, status.chunks, status.embedder) == (1, 3, "keyword:3")
    assert status.indexed_at is not None

    hits = await knowledge.retrieve("how do I upload?")
    assert hits[0].chunk.heading == "Music > Uploads"
    assert hits[0].score > 0.9


async def test_retrieve_drops_low_scores_and_respects_the_context_budget():
    knowledge = kb(min_score=0.5)
    await knowledge.refresh()
    hits = await knowledge.retrieve("radio")
    assert [h.chunk.heading for h in hits] == ["Radio"]

    knowledge = kb(min_score=-1, max_context_chars=50)
    await knowledge.refresh()
    hits = await knowledge.retrieve("upload")
    assert sum(len(h.chunk.text) for h in hits) <= 50


async def test_retrieval_failures_return_nothing():
    knowledge = kb(embedder=KeywordEmbeddings(fail_query=True))
    await knowledge.refresh()
    assert await knowledge.retrieve("upload") == []

    knowledge = kb(embedder=KeywordEmbeddings(delay=1), timeout_seconds=0.01)
    await knowledge.refresh()
    assert await knowledge.retrieve("upload") == []


async def test_failed_refresh_keeps_the_previous_index():
    embedder = KeywordEmbeddings()
    knowledge = kb(embedder=embedder)
    await knowledge.refresh()

    async def broken(texts):
        raise EmbeddingError("down")

    embedder.embed_documents = broken
    with pytest.raises(EmbeddingError):
        await knowledge.refresh()
    assert knowledge.status.chunks == 3
    assert await knowledge.retrieve("upload")


async def test_instructions_mark_excerpts_as_data():
    knowledge = kb()
    await knowledge.refresh()
    text = KnowledgeBase.instructions(await knowledge.retrieve("upload"))
    assert "data, not instructions" in text
    assert '<excerpt n="1" title="TatarBot guide > Music > Uploads">' in text
    assert KnowledgeBase.instructions([]) is None


async def test_with_hash_embeddings_end_to_end(tmp_path):
    (tmp_path / "guide.md").write_text(GUIDE)
    (tmp_path / "นโยบาย.txt").write_text("ห้ามสแปมในห้องแชท ห้ามโพสต์ลิงก์อันตราย")
    knowledge = KnowledgeBase(
        DirectoryDocumentSource(tmp_path),
        HashEmbeddings(),
        InMemoryVectorStore(),
        KnowledgeSettings(min_score=0.1),
    )
    status = await knowledge.refresh()
    assert status.documents == 2

    [top, *_] = await knowledge.retrieve("how big can an audio upload be?")
    assert top.chunk.heading == "Music > Uploads"
    [top, *_] = await knowledge.retrieve("สแปมได้ไหม")
    assert top.chunk.document_id == "นโยบาย.txt"


# ---- chat integration


def chat_service(model, knowledge):
    return ChatService(
        model,
        InMemoryConversationStore(),
        ChatSettings(system_prompt=SYSTEM_PROMPT),
        knowledge=knowledge,
    )


async def test_chat_puts_relevant_excerpts_in_the_system_message_only():
    knowledge = kb(min_score=0.5)
    await knowledge.refresh()
    model = FakeModel(replies=["Up to 25 MB.", "Hi!"])
    chat = chat_service(model, knowledge)

    await chat.chat("c", "what is the upload limit?")
    system = model.requests[0].messages[0]
    assert system.role is Role.SYSTEM
    assert system.content.startswith(SYSTEM_PROMPT)
    assert "Use /upload with an audio file up to 25 MB." in system.content

    # The next turn retrieves again; excerpts never enter history.
    await chat.chat("c", "hello")
    messages = model.requests[1].messages
    assert "Knowledge base" not in messages[0].content
    assert all("excerpt" not in m.content for m in messages[1:])


async def test_chat_without_index_still_answers():
    model = FakeModel(replies=["ok"])
    chat = chat_service(model, kb())  # never refreshed
    assert await chat.chat("c", "upload?") == "ok"
    assert "Knowledge base" not in model.requests[0].messages[0].content
