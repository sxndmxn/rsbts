# Media and tag capability contract

This is the human-readable form of capability matrix version 2, exposed by
`rsbts::media::FORMAT_CAPABILITY_VERSION` and
`rsbts::media::format_capabilities()`. A tuple not listed here is not eligible
for native tag projection. Unknown media remains catalogable as an opaque
asset.

Every listed tuple is exercised against a real fixture by
`every_advertised_tuple_has_a_golden_native_round_trip` under all four output
profiles. The test proves content-derived probing, native read/write,
multivalue behavior as advertised, front-artwork behavior as advertised,
preservation of unknown native metadata, reread validation, and unchanged
audio essence.

| Container | Codec | Native dialect | Read | Write | Native multivalue | Artwork | Unknown data | Essence validation |
|---|---|---|:---:|:---:|:---:|:---:|:---:|---|
| FLAC | FLAC | Vorbis comments | yes | yes | yes | yes | preserve | decoded PCM |
| MPEG | MP3 | ID3v2 | yes | yes | no | yes | preserve | decoded PCM |
| Ogg | Vorbis | Vorbis comments | yes | yes | yes | yes | preserve | decoded PCM |
| Ogg | Opus | Vorbis comments | yes | yes | yes | yes | preserve | encoded audio packets |
| Ogg | Speex | Vorbis comments | yes | yes | yes | yes | preserve | encoded audio packets |
| MP4 | AAC | MP4 ilst | yes | yes | yes | yes | preserve | decoded PCM |
| MP4 | ALAC | MP4 ilst | yes | yes | yes | yes | preserve | decoded PCM |
| ADTS | AAC | ID3v2 | yes | yes | no | yes | preserve | encoded audio packets |
| WAVE/BWF | PCM | ID3v2 | yes | yes | no | yes | preserve | decoded PCM |
| AIFF | PCM | ID3v2 | yes | yes | no | yes | preserve | decoded PCM |
| WavPack | WavPack | APEv2 | yes | yes | yes | yes | preserve | encoded audio packets |
| APE | Monkey's Audio | APEv2 | yes | yes | yes | yes | preserve | encoded audio packets |
| Musepack | Musepack | APEv2 | yes | yes | yes | yes | preserve | encoded audio packets |

“No” under native multivalue means the profile projects a deterministic,
reversible display representation appropriate to that dialect; the canonical
catalog remains multivalued. WAVE/BWF and AIFF also recognize their native text
dialects while using ID3v2 as the full projection target.

## Output profiles

- `archival-native-rich`: retain the richest native representation and all
  unknown data.
- `picard-navidrome`: project MusicBrainz/Picard-compatible multivalue and
  identifier conventions for modern players.
- `id3v23-legacy`: use ID3v2.3-compatible separators and encodings without
  discarding the retained original.
- `portable-player`: emit a conservative player-facing subset while canonical
  and unknown metadata remain retained.

Projection is always plan-first and journaled. It writes a sibling temporary
file, syncs it, rereads and validates tags, compares audio essence, and uses
no-clobber publication. A validation failure leaves the original in place and
records recoverable evidence.
