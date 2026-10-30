# Using TatarBot

TatarBot is this community's Discord bot. Mention it (@TatarBot) or reply to one of its
messages to chat; it answers in Thai or English, whichever you wrote in, and can search the
web. In a voice channel it plays radio stations, direct audio links, and songs members
uploaded to the server's library.

## Chat

- Mention the bot or reply to one of its messages. Long answers are split into several
  messages (Discord allows 2,000 characters per message).
- The bot remembers the recent messages of a conversation, but forgets everything when it
  restarts.
- With web search on, answers about current events end with a "Sources" list of links.

## Music

You must be in a voice channel. To control music that is already playing (skip, stop) you
must be in the same channel as the bot.

### Slash commands

- `/radio <station>` plays a radio station and replaces the queue.
- `/play <link or title>` plays a direct http(s) audio link, or the best match from this
  server's library; titles autocomplete.
- `/skip` skips the current track, `/stop` stops and clears the queue (the bot stays in the
  channel), `/queue` shows what is playing, `/leave` makes the bot leave the channel.

### Music from chat

You can also just ask: "@TatarBot put on some jazz", "@TatarBot เปิดเพลงชิลๆ หน่อย",
"skip this", "what's playing?". The bot can play a station or a library song by title,
skip, stop, or show the queue. Leaving, links, uploads and deletes are slash commands only.
Music from chat does not work in direct messages.

### Radio stations

| Station | What it plays |
|---|---|
| `lofi` | Instrumental hip-hop and lo-fi beats |
| `jazz` | Modern jazz |
| `chill` | Ambient and downtempo |
| `ambient` | Atmospheric ambient |
| `lounge` | Spy lounge |

The stations are SomaFM channels, a listener-supported internet radio.

### Uploading your own music

- `/upload <file> [title]` adds an audio attachment to the server's library. Files can be up
  to 25 MB and 15 minutes long.
- `/library [page]` lists the library, 10 songs per page, with the storage used and the
  server's quota.
- `/delete <track>` removes a song. Only the person who uploaded it, or members with the
  upload role, can delete it.
- Play an uploaded song with `/play <title>` or by asking in chat.
