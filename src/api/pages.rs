use rustypipe::{
    client::RustyPipeQuery,
    model::{AlbumType, MusicGenreItem, MusicItem, TrackItem, traits::FromYtItem},
    param::{AlbumOrder, search_filter::MusicSearchFilter},
};

use super::{
    Api, Card, CardKind, CardLayout, Header, More, MoreTracks, PLAYLIST_LIMIT, Page, QueueMode,
    Result, Route, SHELF_SONGS, SearchKind, Section,
    convert::{
        ALBUMS, CARD_THUMB, HEADER_THUMB, ROW_THUMB, album_card, album_type, artist_card,
        artist_refs, card_section, genre_card, join_artists, pick_thumb, playlist_card,
        playlist_tracks, saved_albums, track, track_section, tracks,
    },
    err,
    format::{compact, playlist_meta, songs, total_time},
    home_feed,
};

const HOME_SHELF: usize = 20;
const HOME_GENRES: usize = 12;

fn genre_section(genres: Vec<MusicGenreItem>) -> Option<Section> {
    card_section(
        "Browse moods & genres",
        genres
            .into_iter()
            .take(HOME_GENRES)
            .map(genre_card)
            .collect(),
        CardLayout::Chips,
    )
}

fn sections_page(sections: Vec<Section>) -> Page {
    Page {
        sections,
        ..Default::default()
    }
}

/// Lower than songs', as a grid holds all its art, outside the cache's budget.
const SEARCH_CARDS: usize = 60;

fn no_results(query: &str) -> String {
    format!("No results for \u{201c}{query}\u{201d}")
}

fn search_filter(kind: SearchKind) -> MusicSearchFilter {
    match kind {
        SearchKind::Songs => MusicSearchFilter::Tracks,
        SearchKind::Videos => MusicSearchFilter::Videos,
        SearchKind::Artists => MusicSearchFilter::Artists,
        SearchKind::Albums => MusicSearchFilter::Albums,
        // YouTube Music's own, as the combined page's.
        SearchKind::Playlists => MusicSearchFilter::YtmPlaylists,
    }
}

/// An artist, album or playlist as its card; anything else has none.
fn music_card(item: MusicItem) -> Option<Card> {
    match item {
        MusicItem::Artist(a) => Some(artist_card(a)),
        MusicItem::Album(a) => Some(album_card(a)),
        MusicItem::Playlist(p) => Some(playlist_card(p)),
        _ => None,
    }
}

/// Adds one kind's search results to its section, and says whether they added
/// any and there is room for more, so a batch adding none ends the list.
fn add_results(section: &mut Section, items: Vec<MusicItem>) -> bool {
    fn grow<T>(list: &mut Vec<T>, new: impl Iterator<Item = T>, cap: usize) -> bool {
        let before = list.len();
        list.extend(new);
        (before + 1..cap).contains(&list.len())
    }
    let items = items.into_iter();
    match section {
        Section::Tracks { tracks, .. } => grow(
            tracks,
            items.filter_map(TrackItem::from_ytm_item).map(track),
            PLAYLIST_LIMIT,
        ),
        Section::Cards { cards, .. } => grow(cards, items.filter_map(music_card), SEARCH_CARDS),
    }
}

impl Api {
    pub async fn page(&self, route: &Route, signed_in: bool) -> Result<Page> {
        match route {
            Route::Home => self.home(signed_in).await,
            Route::Explore => self.explore().await,
            Route::Library => self.library().await,
            Route::History => self.history().await,
            Route::Downloads | Route::LocalFiles => Err("only on this PC".into()),
            Route::Search(query, None) => self.search(query).await,
            Route::Search(query, Some(kind)) => self.search_kind(query, *kind).await,
            Route::Album(id) => self.album(id).await,
            Route::Playlist(id) => self.playlist(id).await,
            Route::Artist(id) => self.artist(id).await,
            Route::Genre(id) => self.genre(id).await,
        }
    }

    /// The moods & genres list, kept for the session once fetched. A failure
    /// isn't kept, so the next page to want it asks again.
    async fn genres(&self, q: &RustyPipeQuery) -> Result<Vec<MusicGenreItem>> {
        self.genres
            .get_or_try_init(|| async { q.music_genres().await.map_err(err) })
            .await
            .cloned()
    }

    /// The personal feed when signed in, otherwise the charts. Should the feed
    /// fail before its first shelf, the app decides what stands in for it.
    ///
    /// The first Home sends last run's visitor ID rather than wait for a fresh
    /// one; everything else keeps rustypipe's rotation.
    async fn home(&self, signed_in: bool) -> Result<Page> {
        let saved = self.saved_visitor.lock().ok().and_then(|mut v| v.take());
        if signed_in {
            // The feed's retries cover refusals, so another round with a fresh
            // ID would only hold back the saved feed standing in.
            return self.personal_home(saved.as_deref()).await;
        }
        if let Some(visitor) = saved {
            // A set ID stops rustypipe drawing another on a refusal.
            match self.public_home(&self.q().visitor_data(visitor)).await {
                Ok(page) => return Ok(page),
                Err(e) => log::debug!("home with last run's visitor ID failed: {e}"),
            }
        }
        self.public_home(&self.q()).await
    }

    async fn personal_home(&self, visitor: Option<&str>) -> Result<Page> {
        let q = self.user_q();
        let mut feed = home_feed::Feed::first(&q, visitor).await?;
        let mut sections = feed.release();
        while sections.is_empty() && !feed.done() {
            feed.next(&q).await?;
            sections = feed.release();
        }
        if sections.is_empty() {
            return Err("the personal home feed was empty".into());
        }
        Ok(Page {
            header: None,
            sections,
            more: (!feed.done()).then_some(More::Shelves(feed)),
            ..Default::default()
        })
    }

    async fn public_home(&self, q: &RustyPipeQuery) -> Result<Page> {
        let genres = async { genre_section(self.genres(q).await.unwrap_or_default()) };
        let (charts, new_albums, genres) =
            tokio::join!(q.music_charts(None), q.music_new_albums(), genres);
        let charts = charts.map_err(err)?;

        // Chart track lists are empty nowadays; the Trending playlist has them.
        let trending = match charts
            .playlists
            .iter()
            .find(|p| p.name.starts_with("Trending"))
        {
            Some(p) => q
                .music_playlist(&p.id)
                .await
                .map(|p| tracks(p.tracks.items))
                .unwrap_or_default(),
            None => tracks(charts.trending_tracks.clone()),
        };

        let mut sections = Vec::new();
        sections.extend(track_section(
            "Trending",
            trending.into_iter().take(SHELF_SONGS).collect(),
            QueueMode::Section,
        ));
        sections.extend(card_section(
            "New releases",
            new_albums
                .unwrap_or_default()
                .into_iter()
                .take(HOME_SHELF)
                .map(album_card)
                .collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Top artists",
            charts
                .artists
                .into_iter()
                .take(HOME_SHELF)
                .map(artist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        sections.extend(genres);
        Ok(sections_page(sections))
    }

    async fn explore(&self) -> Result<Page> {
        let q = self.q();
        let (genres, new_albums, new_videos) =
            tokio::join!(self.genres(&q), q.music_new_albums(), q.music_new_videos());
        let genres = genres?;
        let (moods, genres): (Vec<_>, Vec<_>) = genres.into_iter().partition(|g| g.is_mood);
        let mut sections = Vec::new();
        sections.extend(card_section(
            "Moods & moments",
            moods.into_iter().map(genre_card).collect(),
            CardLayout::Chips,
        ));
        sections.extend(card_section(
            "Genres",
            genres.into_iter().map(genre_card).collect(),
            CardLayout::Chips,
        ));
        sections.extend(card_section(
            "New albums & singles",
            new_albums
                .unwrap_or_default()
                .into_iter()
                .map(album_card)
                .collect(),
            CardLayout::Grid,
        ));
        sections.extend(track_section(
            "New music videos",
            tracks(new_videos.unwrap_or_default())
                .into_iter()
                .take(HOME_SHELF)
                .collect(),
            QueueMode::Radio,
        ));
        Ok(sections_page(sections))
    }

    /// The account's YouTube Music history, newest first, so plays on other
    /// devices and the website show here too.
    // shortcut: only the first page YouTube sends; follow its continuation
    // if that turns out shorter than people scroll.
    async fn history(&self) -> Result<Page> {
        let history = self.user_q().music_history().await.map_err(err)?;
        let tracks = tracks(history.items.into_iter().map(|h| h.item).collect());
        if tracks.is_empty() {
            return Err("Songs you play will show up here.".into());
        }
        Ok(Page {
            header: Some(Header {
                label: "Recently played".to_owned(),
                title: "History".to_owned(),
                // No count: with only the first page, it wouldn't be the
                // whole history's.
                ..Default::default()
            }),
            sections: vec![Section::Tracks {
                title: String::new(),
                tracks,
                mode: QueueMode::Section,
                numbered: false,
            }],
            ..Default::default()
        })
    }

    async fn library(&self) -> Result<Page> {
        let q = self.q();
        let (playlist_cards, albums, artists) = tokio::join!(
            self.library_playlists(),
            q.music_saved_albums(),
            q.music_saved_artists(),
        );
        let playlist_cards = playlist_cards?;
        // The rest is the app's to ask for or drop, as only ordering needs it.
        let (albums, more, albums_cut_short) = match albums {
            Ok(mut rest) => {
                let albums = std::mem::take(&mut rest.items);
                let cut_short = !rest.is_exhausted();
                // Only a list shown has a section for the rest to join.
                let more = (cut_short && !albums.is_empty()).then_some(More::Albums(rest));
                (albums, more, cut_short)
            }
            Err(_) => (Vec::new(), None, false),
        };
        let mut sections = Vec::new();
        sections.extend(card_section("Playlists", playlist_cards, CardLayout::Grid));
        sections.extend(card_section(
            ALBUMS,
            albums.into_iter().map(album_card).collect(),
            CardLayout::Grid,
        ));
        sections.extend(card_section(
            "Artists",
            artists
                .map(|p| p.items)
                .unwrap_or_default()
                .into_iter()
                .map(artist_card)
                .collect(),
            CardLayout::Grid,
        ));
        Ok(Page {
            sections,
            more,
            albums_cut_short,
            ..Default::default()
        })
    }

    /// Liked music followed by the saved playlists, for the sidebar.
    pub async fn library_playlists(&self) -> Result<Vec<Card>> {
        let q = self.q();
        let (playlists, liked) = tokio::join!(q.music_saved_playlists(), q.music_liked_tracks());
        let playlists = playlists.map_err(|e| format!("could not load your library: {e}"))?;
        let mut cards = Vec::new();
        if let Ok(liked) = &liked {
            cards.push(Card {
                kind: CardKind::Playlist,
                id: liked.id.clone(),
                title: "Liked music".into(),
                subtitle: liked.track_count.map_or_else(
                    || "Auto playlist".into(),
                    |n| format!("Auto playlist • {}", songs(n)),
                ),
                thumb: pick_thumb(&liked.thumbnail, CARD_THUMB),
                color: None,
                track: None,
            });
        }
        let liked_id = liked.as_ref().ok().map(|l| l.id.as_str());
        cards.extend(
            playlists
                .items
                .into_iter()
                .filter(|p| liked_id != Some(p.id.as_str()))
                // Episodes for Later is a podcast page, not a playlist.
                .filter(|p| p.id != "SE")
                .map(playlist_card),
        );
        Ok(cards)
    }

    async fn search(&self, query: &str) -> Result<Page> {
        let q = self.q();
        let (main, songs, videos, albums, artists, playlists) = tokio::join!(
            q.music_search_main(query),
            q.music_search_tracks(query),
            q.music_search_videos(query),
            q.music_search_albums(query),
            q.music_search_artists(query),
            q.music_search_playlists(query, false),
        );
        let songs = songs.map_err(err)?;

        let mut sections = Vec::new();
        let top = main
            .ok()
            .and_then(|m| m.items.items.into_iter().next())
            .and_then(music_card);
        if let Some(card) = top {
            sections.extend(card_section("Top result", vec![card], CardLayout::Top));
        }
        sections.extend(track_section(
            "Songs",
            tracks(songs.items.items),
            QueueMode::Radio,
        ));
        // Uploads too: slowed and sped-up edits, mashups and live versions
        // are videos, not songs, on YouTube Music.
        sections.extend(track_section(
            "Videos",
            tracks(videos.map(|r| r.items.items).unwrap_or_default()),
            QueueMode::Radio,
        ));
        sections.extend(card_section(
            "Artists",
            artists
                .map(|r| r.items.items)
                .unwrap_or_default()
                .into_iter()
                .map(artist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Albums",
            albums
                .map(|r| r.items.items)
                .unwrap_or_default()
                .into_iter()
                .map(album_card)
                .collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Playlists",
            playlists
                .map(|r| r.items.items)
                .unwrap_or_default()
                .into_iter()
                .map(playlist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        if sections.is_empty() {
            return Err(no_results(query));
        }
        Ok(sections_page(sections))
    }

    /// One kind's results alone: its first page now, the rest through
    /// `complete`.
    async fn search_kind(&self, query: &str, kind: SearchKind) -> Result<Page> {
        let mut results = self
            .q()
            .music_search::<MusicItem, _>(query, Some(search_filter(kind)))
            .await
            .map_err(err)?
            .items;
        let mut section = if kind.songs() {
            Section::Tracks {
                title: String::new(),
                tracks: Vec::new(),
                mode: QueueMode::Radio,
                numbered: false,
            }
        } else {
            Section::Cards {
                title: String::new(),
                cards: Vec::new(),
                layout: CardLayout::Grid,
            }
        };
        let room = add_results(&mut section, std::mem::take(&mut results.items));
        if section.is_empty() {
            return Err(no_results(query));
        }
        Ok(Page {
            header: None,
            sections: vec![section],
            more: (room && results.ctoken.is_some()).then_some(More::Search(results)),
            ..Default::default()
        })
    }

    pub(super) async fn album(&self, id: &str) -> Result<Page> {
        // As the account sees it, for whether it is saved.
        let album = self
            .as_user(|q: RustyPipeQuery| async move { q.music_album(id).await })
            .await?;
        let cover = pick_thumb(&album.cover, ROW_THUMB);
        let mut tracks = tracks(album.tracks);
        for t in &mut tracks {
            if t.thumb.is_none() {
                t.thumb.clone_from(&cover);
            }
            if t.album.is_none() {
                t.album = Some((album.id.clone(), album.name.clone()));
            }
        }
        let mut meta = Vec::new();
        if let Some(year) = album.year {
            meta.push(year.to_string());
        }
        meta.push(songs(tracks.len() as u64));
        meta.extend(total_time(&tracks));

        let header = Header {
            label: album_type(album.album_type).to_owned(),
            title: album.name,
            subtitle: join_artists(&artist_refs(&album.artists)),
            meta: meta.join(" • "),
            thumb: pick_thumb(&album.cover, HEADER_THUMB),
            round: false,
            banner: false,
            shuffle_playlist: None,
            radio_id: album.playlist_id.as_ref().map(|p| format!("RDAMPL{p}")),
            saved: album.saved,
            save_id: album.playlist_id,
            ..Default::default()
        };
        let mut sections = vec![Section::Tracks {
            title: String::new(),
            tracks,
            mode: QueueMode::Section,
            numbered: true,
        }];
        sections.extend(card_section(
            "Other versions",
            album.variants.into_iter().map(album_card).collect(),
            CardLayout::Carousel,
        ));
        Ok(Page {
            header: Some(header),
            sections,
            ..Default::default()
        })
    }

    /// A playlist's first page of tracks; `complete` fetches the rest.
    async fn playlist(&self, id: &str) -> Result<Page> {
        let playlist = self.fetch_playlist(id, false).await?;
        // Only those the account made take edits, not Liked music or the mixes.
        let own = playlist.editable && playlist.id.starts_with("PL");
        let tracks = playlist_tracks(playlist.tracks.items.clone(), own);
        let more = (playlist.tracks.ctoken.is_some() && !tracks.is_empty()).then_some(
            More::Tracks(MoreTracks {
                tracks: playlist.tracks,
                count: playlist.track_count,
            }),
        );
        let header = Header {
            label: "Playlist".to_owned(),
            title: playlist.name,
            subtitle: playlist.channel.map(|c| c.name).unwrap_or_default(),
            meta: playlist_meta(playlist.track_count, &tracks),
            thumb: pick_thumb(&playlist.thumbnail, HEADER_THUMB),
            round: false,
            banner: false,
            shuffle_playlist: None,
            radio_id: Some(format!("RDAMPL{}", playlist.id)),
            count: playlist.track_count,
            editable: own,
            privacy: playlist.privacy.filter(|_| own),
            saved: playlist.saved,
            save_id: playlist.saved.map(|_| playlist.id.clone()),
            ..Default::default()
        };
        let mut sections = Vec::new();
        sections.extend(track_section("", tracks, QueueMode::Section));
        sections.extend(card_section(
            "Related playlists",
            playlist
                .related_playlists
                .items
                .into_iter()
                .map(playlist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        Ok(Page {
            header: Some(header),
            sections,
            more,
            ..Default::default()
        })
    }

    /// Fetches the next step of what `page` has still to show, and leaves
    /// `more` set while there is another. Keeps what arrived should a
    /// request fail, and says whether all of it came.
    pub async fn complete(&self, mut page: Page) -> (Page, bool) {
        let mut whole = true;
        match page.more.take() {
            None => {}
            // The rest of a playlist's tracks, up to `PLAYLIST_LIMIT`, in one go.
            Some(More::Tracks(MoreTracks { mut tracks, count })) => {
                let shown = tracks.items.len();
                whole = tracks.extend_limit(self.q(), PLAYLIST_LIMIT).await.is_ok();
                // The first page has no repeats, so `shown` still marks its end;
                // capped all the same, as `split_off` past the end panics.
                super::drop_repeats(&mut tracks.items);
                let own = page.header.as_ref().is_some_and(|h| h.editable);
                let shown = shown.min(tracks.items.len());
                let extra = playlist_tracks(tracks.items.split_off(shown), own);
                if let Some(Section::Tracks { tracks, .. }) = page.sections.first_mut() {
                    tracks.extend(extra);
                }
                let meta = playlist_meta(count, page.header_tracks());
                if let Some(header) = &mut page.header {
                    header.meta = meta;
                }
            }
            // A page of one kind of search result at a time: songs as their
            // rows near the end, cards as each arrives.
            Some(More::Search(mut results)) => match results.extend(self.q()).await {
                Ok(_) => {
                    let batch = std::mem::take(&mut results.items);
                    if let Some(section) = page.sections.first_mut()
                        && add_results(section, batch)
                        && results.ctoken.is_some()
                    {
                        page.more = Some(More::Search(results));
                    }
                }
                Err(e) => {
                    log::warn!("could not load more search results: {e}");
                    whole = false;
                    // Songs ask again as rows next near the end; cards follow on
                    // at once, so would retry in a loop.
                    if let Some(Section::Tracks { .. }) = page.sections.first() {
                        page.more = Some(More::Search(results));
                    }
                }
            },
            Some(More::Shelves(mut feed)) => {
                if let Err(e) = feed.next(&self.user_q()).await {
                    log::warn!("could not load more of the home feed: {e}");
                    whole = false;
                }
                page.sections.extend(feed.release());
                if !feed.done() {
                    page.more = Some(More::Shelves(feed));
                }
            }
            // A page a step, so leaving Library stops it.
            Some(More::Albums(mut rest)) => {
                let asked = rest.ctoken.clone();
                if let Err(e) = rest.extend(self.q()).await {
                    log::warn!("could not load more of the saved albums: {e}");
                }
                let added = std::mem::take(&mut rest.items);
                let more = !added.is_empty() && rest.ctoken != asked;
                if let Some(cards) = saved_albums(&mut page.sections) {
                    cards.extend(added.into_iter().map(album_card));
                }
                if rest.is_exhausted() {
                    page.albums_cut_short = false;
                } else if more {
                    page.more = Some(More::Albums(rest));
                } else {
                    // Failed, or a page that adds nothing or points back at
                    // itself, which would ask again for ever.
                    whole = false;
                }
            }
        }
        (page, whole)
    }

    async fn artist(&self, id: &str) -> Result<Page> {
        // Together rather than one after the other as rustypipe does; the
        // page's own albums stand in if the discography fails or adds nothing.
        let q = self.q();
        let (artist, discography) = tokio::join!(
            // As the account sees it, for whether it is subscribed.
            self.as_user(|q: RustyPipeQuery| async move { q.music_artist(id, false).await }),
            q.music_artist_albums(id, None, Some(AlbumOrder::Recency)),
        );
        let mut artist = artist?;
        // An alias's page redirects to the artist's own ID, but its discography
        // is already the artist's, so it is asked again only should it fail or
        // add nothing.
        let adds = matches!(&discography, Ok(all) if all.len() > artist.albums.len());
        let discography = if artist.id == id || adds {
            discography
        } else {
            q.music_artist_albums(&artist.id, None, Some(AlbumOrder::Recency))
                .await
        };
        match discography {
            Ok(all) if all.len() > artist.albums.len() => artist.albums = all,
            Ok(_) => {}
            Err(e) => log::warn!("could not load the artist's discography: {e}"),
        }
        let top = tracks(artist.tracks);
        let header = Header {
            label: "Artist".to_owned(),
            title: artist.name,
            subtitle: artist
                .subscriber_count
                .map(|n| format!("{} subscribers", compact(n)))
                .unwrap_or_default(),
            meta: String::new(),
            // Wide banners must keep their aspect, so take the largest as is.
            thumb: artist
                .header_image
                .iter()
                .max_by_key(|t| t.width)
                .map(|t| t.url.clone()),
            round: true,
            banner: true,
            shuffle_playlist: artist.tracks_playlist_id,
            radio_id: artist.radio_id,
            subscribed: artist.subscribed,
            channel_id: artist.subscribe_channel_id,
            artist_id: Some(artist.id),
            ..Default::default()
        };

        let (singles, albums): (Vec<_>, Vec<_>) = artist
            .albums
            .into_iter()
            .partition(|a| matches!(a.album_type, AlbumType::Single | AlbumType::Ep));
        let mut sections = Vec::new();
        sections.extend(track_section("Songs", top, QueueMode::Section));
        sections.extend(card_section(
            "Albums",
            albums.into_iter().map(album_card).collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Singles & EPs",
            singles.into_iter().map(album_card).collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Featured on",
            artist.playlists.into_iter().map(playlist_card).collect(),
            CardLayout::Carousel,
        ));
        sections.extend(card_section(
            "Fans might also like",
            artist
                .similar_artists
                .into_iter()
                .map(artist_card)
                .collect(),
            CardLayout::Carousel,
        ));
        Ok(Page {
            header: Some(header),
            sections,
            ..Default::default()
        })
    }

    async fn genre(&self, id: &str) -> Result<Page> {
        let genre = self.q().music_genre(id).await.map_err(err)?;
        let sections = genre
            .sections
            .into_iter()
            .filter_map(|s| {
                card_section(
                    &s.name,
                    s.playlists.into_iter().map(playlist_card).collect(),
                    CardLayout::Carousel,
                )
            })
            .collect();
        Ok(Page {
            header: Some(Header {
                label: "Genre".to_owned(),
                title: genre.name,
                ..Default::default()
            }),
            sections,
            ..Default::default()
        })
    }
}
