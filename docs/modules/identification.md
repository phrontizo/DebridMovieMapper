# `identification.rs`

Filename cleaning, camelCase splitting, TMDB scoring to identify movies/shows.
A leading `site.tld - ` prefix (any 2-6-letter TLD) is stripped only when it looks like a site (`site_match_is_a_site`).
A `www.` name always does.
Any other dotted name before ` - ` is kept as title content when it carries a release token itself (a year, resolution, SxxEyy or source/codec tag, `RELEASE_TOKEN_RE`: `Show.Name.S01E01.720p.HDTV - Group`), when the text after the ` - ` starts with an episode code, a year or a bare number (SxxEyy, Sxx, Exx, Ep01, NxM, Season N, Episode N, `Dr.No - 1962`, `Show.Name - 101`; `SITE_FOLLOWED_BY_EPISODE_RE`), or when no release token follows it (`Mr.Bean - The Movie`), since a site prefix is followed by a release name.
An `NxM` episode code (`1x01`) ends the title as an SxxEyy one does, except at the very start (a film titled `10x10`).
`is_show_guess`'s markers run with `_` read as a space (`Some_Show_S01E05_720p` is a show), and the short episode word `Ep 5` / `Ep.05` counts; a bare `Episode N` doesn't (films use it).
`identify_name_with_show_hint` lets the acquisition title check of a show request settle the show-versus-film choice; movie identification and the account mirror pass no hint.
`names_a_title` tells a file or folder name with a title of its own from one that opens with episode numbering (`E09 - Title`, `S01E05`, `05 - Title`, `Season 04`).
`fallback_metadata` is the TMDB-free identification `identify_torrent` falls back to when nothing matched
