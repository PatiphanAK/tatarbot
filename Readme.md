# Tatarbot

```text
Discord ──▶ discord_bot/ (Rust) ──POST /v1/chat──▶ agent_runtime/ (Python) ──▶ vLLM / Qwen
```

* `discord_bot/` is the Discord adapter. It only knows the Agent Runtime's `/v1/chat` contract.
* `agent_runtime/` handles conversation history, the system prompt and the model port. It
  only knows the inference backend.

```sh
make up      # start the runtime on :8080, then the bot
make check   # lint and test both
```
