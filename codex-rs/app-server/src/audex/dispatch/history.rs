//! Original reviewer evidence from Codex's paginated, uncompacted display history.
use super::*;
use codex_app_server_protocol::ThreadItemPayload;
use codex_thread_store::ItemSortKey;
use codex_thread_store::ListItemsParams;
use codex_thread_store::ListItemsPosition;
use codex_thread_store::ListTurnsParams;
use codex_thread_store::SortDirection;
use codex_thread_store::StoredTurnItemsView;
use codex_thread_store::StoredTurnStatus;

impl CodexDispatch {
    pub(super) async fn original_evidence(
        &self,
        operation: &str,
        thread_id: ThreadId,
        turn: &str,
    ) -> Result<Option<WorkState>, DispatchFailure> {
        if !self.history.supports_paginated_history_lists() {
            return Ok(None);
        }
        let store = self
            .store()
            .await
            .map_err(|e| DispatchFailure::new("open_store", e, false))?;
        let mut position = None;
        let mut messages = Vec::new();
        loop {
            let page = self
                .history
                .list_items(ListItemsParams {
                    thread_id,
                    turn_id: Some(turn.to_owned()),
                    item_ids: None,
                    include_archived: false,
                    position,
                    page_size: 256,
                    sort_direction: SortDirection::Asc,
                    sort_key: ItemSortKey::CreatedAtOrdinal,
                    after_updated_at_ordinal: None,
                    before_updated_at_ordinal: None,
                })
                .await
                .map_err(|e| DispatchFailure::new("read_original_items", e, false))?;
            for item in page.items {
                let payload: ThreadItemPayload = serde_json::from_slice(&item.item_json)
                    .map_err(|e| DispatchFailure::new("decode_original_item", e, false))?;
                if let ThreadItemPayload::AgentMessage { text, .. } = payload {
                    if !text.is_empty() {
                        messages.push((item.item_id, text));
                    }
                }
            }
            let Some(cursor) = page.next_cursor else {
                break;
            };
            position = Some(ListItemsPosition::Cursor(cursor));
        }
        store
            .retain_recovered_native_output(operation, &thread_id.to_string(), turn, &messages)
            .await
            .map_err(|e| DispatchFailure::new("retain_original_items", e, false))?;
        #[cfg(test)]
        if self
            .hooks
            .original_terminal_visible
            .as_ref()
            .is_some_and(|visible| !visible())
        {
            return Ok(None);
        }
        let mut cursor = None;
        loop {
            let page = self
                .history
                .list_turns(ListTurnsParams {
                    thread_id,
                    include_archived: false,
                    cursor,
                    page_size: 256,
                    sort_direction: SortDirection::Desc,
                    items_view: StoredTurnItemsView::Summary,
                })
                .await
                .map_err(|e| DispatchFailure::new("read_original_turn", e, false))?;
            for stored in page.turns {
                if stored.turn_id != turn {
                    continue;
                }
                return match stored.status {
                    StoredTurnStatus::InProgress => Ok(None),
                    StoredTurnStatus::Interrupted => Ok(Some(WorkState::Interrupted)),
                    StoredTurnStatus::Failed => Ok(Some(WorkState::Failed(
                        json!({
                            "version":1,"stage":"reviewer_turn",
                            "cause":stored.error.and_then(|error| error.codex_error_info),
                        })
                        .to_string(),
                    ))),
                    StoredTurnStatus::Completed => {
                        // Codex's summary projects the actual final agent item,
                        // independently of later compaction of model context.
                        let mut final_text = String::new();
                        for item in stored.items {
                            let payload: ThreadItemPayload =
                                serde_json::from_slice(&item.item_json).map_err(|e| {
                                    DispatchFailure::new("decode_original_final", e, false)
                                })?;
                            if let ThreadItemPayload::AgentMessage { text, .. } = payload {
                                final_text = text;
                            }
                        }
                        Ok(Some(WorkState::Completed(final_text.into_bytes())))
                    }
                };
            }
            let Some(next) = page.next_cursor else {
                return Ok(None);
            };
            cursor = Some(next);
        }
    }
}
