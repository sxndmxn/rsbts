# Media and tag capability contract

This is the human-readable form of capability matrix version 2, exposed by
`rsbts::media::FORMAT_CAPABILITY_VERSION` and
`rsbts::media::format_capabilities()`. A tuple not listed here is not eligible
for native tag projection. Unknown media remains catalogable as an opaque
asset.

Each listed tuple has a fixture test under all four output profiles.
`every_advertised_tuple_has_a_golden_native_round_trip` checks content probing,
native reads and writes, and the fixture's repeated values.
It also checks fixture artwork, unknown metadata, reread validation, and audio preservation.

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

Native multivalue support applies to the tested fields.
Support varies by metadata key and format.
“No” means the profile writes a deterministic display value for the tested fields.
The canonical catalog retains its separate values.
The reader does not split display text heuristically. It splits APE NUL separators.
WAVE/BWF and AIFF also recognize their native text dialects and use ID3v2 for tag projection.

The rich FLAC workflow preserves repeated credits, release facts, classical fields,
typed MusicBrainz identifiers, calendar dates, totals, and ReplayGain text.
The WavPack/APEv2 fixture covers these fields without a movement total.
Its existing movement-total mapping produces duplicate movement-number values, which the reader rejects.
The rich MP4 AAC fixture excludes performers because the native mapping cannot represent them.
Existing ID3 projection rejects performers, generic MusicBrainz recording-ID items, and `WORK` frames.
The basic ID3 fixture tests still pass.

Calendar dates accept `YYYY`, `YYYY-MM`, or `YYYY-MM-DD`.
Empty native fields represent unknown values.
Native dates `0` and `0000` also represent an unknown value.
Zero positions and totals also represent unknown values.
Repeated scalar values are accepted when every value agrees.
Repeated native list values retain one copy of each exact value, in source order.
The reader rejects finer timestamp precision, malformed typed values, and conflicting scalar values.
These cases require metadata review before import or update.
Native tag readers accept up to 64 KiB of padding between ID3 tags and audio frames.
Import scans report unreadable files with recognized audio extensions, including unsupported formats such as WMA.
ID3 tag writes can fail when padding after the tag exceeds 1 KiB.
The failed write leaves the file unchanged.

FLAC tag writes currently reject files with empty fields outside the approved tag changes.
The tag library drops those entries.
The preservation check blocks replacement and leaves the original file unchanged.

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
