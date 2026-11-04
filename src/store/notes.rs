use super::*;

impl Store {
    // ---- notes ----

    pub fn announce(&mut self, text: &str, error: bool, cx: &mut Context<Self>) {
        // land in the focused transcript, else the first tile
        let target = self
            .focused_tile
            .clone()
            .or_else(|| self.open_tiles.first().map(|t| t.key.clone()));
        if let Some(target) = target {
            self.tile_note(&target, text, error, cx);
        }
    }

    /// Same note, but only into a tile that actually talks to `ws_key` — a
    /// server's connect/status notes must not surface in a chat that runs on
    /// another server (a dsh draft must not greet with opencode's note).
    pub(super) fn announce_for_key(&mut self, ws_key: &str, text: &str, cx: &mut Context<Self>) {
        let matches_key =
            |this: &Self, key: &str| this.tile_net_key_or(key).as_deref() == Some(ws_key);
        let target = self
            .focused_tile
            .clone()
            .filter(|key| matches_key(self, key))
            .or_else(|| {
                self.open_tiles
                    .iter()
                    .map(|t| t.key.clone())
                    .find(|key| matches_key(self, key))
            });
        if let Some(target) = target {
            self.tile_note(&target, text, false, cx);
        }
    }

    pub fn tile_note(&mut self, tile_key: &str, text: &str, error: bool, cx: &mut Context<Self>) {
        let note_key = self.next_key;
        self.next_key += 1;
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.items.push(Arc::new(ChatItem::SystemNote {
                key: note_key,
                text: text.into(),
                is_error: error,
            }));
        }
        self.repaint(cx);
    }

    /// Create or evolve the workspace's single "connection lost" note. A drop
    /// within oc::RECONNECT_RESET_AFTER of the previous event rewrites that
    /// note in place (its attempt count climbs); an older drop starts a new
    /// one, so a stable stretch closes the series.
    pub(super) fn reconnect_note(
        &mut self,
        ws_key: &str,
        text: &str,
        error: bool,
        cx: &mut Context<Self>,
    ) {
        let now = Instant::now();
        let expired = match self.reconnect_notes.get(ws_key) {
            Some((_, _, last)) => now.duration_since(*last) >= oc::RECONNECT_RESET_AFTER,
            None => true,
        };
        if expired {
            let Some(tile_key) = self
                .focused_tile
                .clone()
                .or_else(|| self.open_tiles.first().map(|t| t.key.clone()))
            else {
                return;
            };
            let note_key = self.next_key;
            self.next_key += 1;
            if let Some(tile) = self.tile_by_key_mut(&tile_key) {
                tile.items.push(Arc::new(ChatItem::SystemNote {
                    key: note_key,
                    text: text.into(),
                    is_error: error,
                }));
            }
            self.reconnect_notes
                .insert(ws_key.to_string(), (tile_key.clone(), note_key, now));
            self.touch_tile(&tile_key, cx);
        } else {
            let Some((tile_key, note_key, last)) = self.reconnect_notes.get_mut(ws_key) else {
                return;
            };
            *last = now;
            let (tile_key, note_key) = (tile_key.clone(), *note_key);
            self.rewrite_note(&tile_key, note_key, text, error);
            self.touch_tile(&tile_key, cx);
        }
    }

    /// Flip the workspace's live reconnect note to its settled "reconnected"
    /// text. Only a series with a prior "connection lost" answers: the pump
    /// reports Connected on every attach, including the quiet first one.
    pub(super) fn settle_reconnect_note(&mut self, ws_key: &str, cx: &mut Context<Self>) {
        let Some((tile_key, note_key)) = self
            .reconnect_notes
            .get(ws_key)
            .map(|(t, n, _)| (t.clone(), *n))
        else {
            return;
        };
        self.reconnect_notes.get_mut(ws_key).unwrap().2 = Instant::now();
        self.rewrite_note(&tile_key, note_key, "Event stream reconnected.", false);
        self.touch_tile(&tile_key, cx);
    }

    /// Rewrite one SystemNote's text/severity inside its tile's item list.
    fn rewrite_note(&mut self, tile_key: &str, note_key: usize, text: &str, error: bool) {
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            for item in &mut tile.items {
                if matches!(item.as_ref(), ChatItem::SystemNote { key, .. } if *key == note_key) {
                    if let ChatItem::SystemNote {
                        text: t,
                        is_error: e,
                        ..
                    } = Arc::make_mut(item)
                    {
                        *t = text.into();
                        *e = error;
                    }
                    break;
                }
            }
        }
    }

    pub(super) fn post_note_for_session(&mut self, session_id: &str, text: &str, error: bool) {
        let note_key = self.next_key;
        self.next_key += 1;
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned()
            && let Some(tile) = self.tile_by_key_mut(&tile_key)
        {
            tile.items.push(Arc::new(ChatItem::SystemNote {
                key: note_key,
                text: text.into(),
                is_error: error,
            }));
            return;
        }
        let title = self.title_for(session_id);
        let target = self
            .focused_tile
            .clone()
            .or_else(|| self.open_tiles.first().map(|t| t.key.clone()));
        if let Some(target) = target
            && let Some(tile) = self.tile_by_key_mut(&target)
        {
            tile.items.push(Arc::new(ChatItem::SystemNote {
                key: note_key,
                text: format!("{title}: {text}"),
                is_error: error,
            }));
        }
    }
}
